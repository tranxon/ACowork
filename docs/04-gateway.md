# Gateway 组件详细设计

> 版本：v3.1 | 更新日期：2026-04-14

---

Gateway 是一个常驻的系统级进程，使用 Rust 实现。Gateway **不代理 Agent 的业务逻辑**（不代理 LLM 调用、不代理工具执行），只负责必须集中化的协调工作。

Gateway 同时为两类消费者提供服务：

```
┌──────────────────┐         ┌──────────────────┐
│  Agent Runtime   │         │  Desktop App     │
│  (多个进程)       │         │  / CLI           │
└────────┬─────────┘         └────────┬─────────┘
         │ Socket API                 │ HTTP API
         │ (IPC, 长连接)               │ (REST + WS)
         ▼                            ▼
┌────────────────────────────────────────────────┐
│                Gateway (单进程)                 │
│                                                │
│  ┌─────────────┐  ┌──────────┐  ┌──────────┐  │
│  │ Package Mgr │  │ Lifecycle│  │ Intent   │  │
│  │             │  │ Manager  │  │ Router   │  │
│  ├─────────────┤  ├──────────┤  ├──────────┤  │
│  │ Key Vault   │  │ Budget   │  │ Rate     │  │
│  │             │  │ Tracker  │  │ Limiter  │  │
│  └─────────────┘  └──────────┘  └──────────┘  │
└────────────────────────────────────────────────┘
```

- **Socket API**：给 Agent Runtime 用的 IPC 通道（Unix Socket / Named Pipe）
- **HTTP API**：给 Desktop App / CLI 用的 REST 接口（Axum，localhost only）

两者共享 Gateway 内部状态，只是接入层不同。

## 1. Package Manager

- **安装**：解压 `.agent` 到 `~/.local/share/agent-gateway/agents/<agent_id>/`，校验 manifest 完整性，记录版本。安装前必须验证包签名（详见 [02-agent-package.md](./02-agent-package.md)），签名无效或与已安装版本签名不一致则拒绝安装。
- **卸载**：删除对应目录，可选备份用户数据（含私有 Grafeo）。
- **升级**：保留 `data/` 和用户修改的 `config/`，替换其他文件。若 runtime_version 不兼容则提示用户。升级时校验新包签名证书指纹必须与已安装版本一致。
- **仓库支持**：可配置多个 HTTP 仓库源（类似 apt），定期检查更新。仓库提供的 .agent 包必须经过签名。

## 2. 生命周期管理器

**启动策略：**
- 按需启动：当收到匹配 trigger 的消息或用户显式调用时启动。
- 常驻：用户可标记某 Agent 开机自启。
- 定时启动：由 cron 表达式触发。（**Phase 3 实现**，Phase 2 仅支持按需启动和常驻模式）

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

## 3. Intent Router

**输入源：**
- 用户界面（CLI/GUI）发出的请求。
- 定时任务触发器。
- 其他 Agent 通过 Gateway 转发的 Intent 消息（见 [06-communication.md](./06-communication.md)）。

**路由规则：**
- 根据消息中的 `target` 字段直接路由到目标 Agent。
- 若目标 Agent 未运行，则按需启动。
- 若未指定 target，则匹配已安装 Agent 的 manifest 中 `triggers.message.pattern`。

## 4. 沙箱配置器

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

> WASM 引擎选型详情见 [12-tool-system.md](./12-tool-system.md) 第 3.1 节。
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

## 5. Key Vault

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

## 6. Budget Tracker

接收 Agent Runtime 上报的 LLM 用量，维护跨 Agent 的统计：

- 每个 Agent 有独立的日/月 Token 和费用限额。
- 超限时向 Agent 发送信号（stop / fallback / warn）。
- 提供预算查询接口供 Agent 本地预检。

## 7. Rate Limiter

协调多 Agent 对同一 LLM Provider 的并发请求，避免触发 API Rate Limit：

- Agent 调 LLM 前通过 Gateway 申请速率令牌（极轻量 RPC，< 0.1ms）。
- Gateway 基于 Provider 的 RPM/TPM 限制分配令牌。

## 8. 配置与数据存储

- **Gateway 自身配置**：`~/.config/agent-gateway/config.toml`（含 Vault 配置、仓库列表、默认 LLM 配置等）。
- **每个 Agent 的工作区**：`~/.local/share/agent-gateway/agents/<agent_id>/workspace/`：
  - `data/`：从包中复制，可读写。
  - `config/`：用户可修改的配置（初始来自包内 config）。
  - `memory/`：私有 Grafeo 数据库文件（`private.grafeo`）。
  - `runtime/`：临时文件（socket、pid）。
- **日志**：Gateway 收集所有 Agent 的 stdout/stderr，写入 `~/.local/share/agent-gateway/logs/`，支持按 Agent 过滤。

## 9. HTTP API（Desktop App / CLI 接入层）

> **Phase 2 实现状态**：HTTP API 的完整实现（Axum 路由 + WebSocket 流式）降级为 **Phase 3**。Phase 2 优先实现 Socket IPC（Agent Runtime 通信）。HTTP API 的接口定义和路由设计保留在本章作为 Phase 3 实现参考，接口预留已体现在协议设计中（见 §3 Socket API）。

Socket API 是面向 Agent Runtime 进程间通信设计的二进制帧协议，不适合 WebView 直接调用或 CLI 工具消费。为此，Gateway 新增一层 HTTP API，作为 Desktop App 和 CLI 的统一接入点。

### 9.1 为什么需要双 API 层

| 维度 | Socket API | HTTP API |
|------|-----------|----------|
| 消费者 | Agent Runtime | Desktop App / CLI |
| 传输层 | Unix Socket / Named Pipe | HTTP (localhost) |
| 通信模式 | 长连接 + 双向推送 | 请求/响应 + WebSocket 流式 |
| 帧格式 | 自定义二进制帧（4B 长度 + 1B 类型 + JSON） | 标准 HTTP/JSON + WebSocket |
| 认证方式 | 进程级信任（本地 IPC） | localhost only + 可选 token |
| 用途 | 进程间实时通信（Key 分发、Intent、预算） | 用户界面操作（Agent 管理、对话、配置） |

两者共享 Gateway 内部逻辑（Package Manager、Lifecycle Manager 等），只是接入层不同。HTTP API 是 Socket API 之上的一层薄封装，不引入新的业务逻辑。

### 9.2 HTTP Server 配置

```rust
// Gateway 进程启动时同时监听两个端口：
// 1. Socket API（给 Agent Runtime 用）
// 2. HTTP API（给 Desktop App / CLI 用）

pub struct HttpConfig {
    /// 监听地址，默认 127.0.0.1
    pub host: String,
    /// 监听端口，默认 19876
    pub port: u16,
    /// 是否启用 CORS（开发模式），默认 false
    pub cors_enabled: bool,
}
```

- 默认监听 `http://127.0.0.1:19876`，仅 localhost，不对外暴露
- 端口可在 `config.toml` 中配置
- 端口冲突时自动递增尝试（19876 → 19877 → 19878...），最终端口写入 pidfile 供 Desktop App 发现

### 9.3 路由定义

```rust
use axum::{Router, routing::{get, post, put, delete}, extract::WebSocketUpgrade};

pub fn http_routes() -> Router<GatewayState> {
    Router::new()
        // 健康检查
        .route("/health", get(health_check))

        // --- Agent 管理 ---
        .route("/api/agents", get(list_agents))
        .route("/api/agents/:id", get(get_agent_detail))
        .route("/api/agents/install", post(install_agent))         // body: { path: String }
        .route("/api/agents/:id", delete(uninstall_agent))
        .route("/api/agents/:id/clone", post(clone_agent))         // body: { mode, new_id }
        .route("/api/agents/:id/start", post(start_agent))
        .route("/api/agents/:id/stop", post(stop_agent))

        // --- 对话 ---
        .route("/api/agents/:id/message", post(send_message))      // body: { content: String }
        .route("/api/agents/:id/stream", get(agent_stream_ws))     // WebSocket 升级

        // --- Vault ---
        .route("/api/vault/keys", get(list_keys))
        .route("/api/vault/keys", post(add_key))                   // body: { provider, key }
        .route("/api/vault/keys/:provider", delete(remove_key))
        .route("/api/vault/keys/:provider", put(update_key))       // body: { key: String }

        // --- 配置 ---
        .route("/api/config", get(get_config))
        .route("/api/config", put(update_config))                  // body: { ... }

        // --- 系统信息 ---
        .route("/api/status", get(system_status))

        // --- 发布 ---
        .route("/api/agents/:id/publish/prepare", post(publish_prepare))
        .route("/api/agents/:id/publish/build", post(publish_build))
        .route("/api/agents/:id/publish/install-locally", post(publish_install_locally))
        .route("/api/agents/:id/publish/export", post(publish_export))
}
```

### 9.4 核心接口详情

#### 9.4.1 Agent 管理

```json
// GET /api/agents
// → 200
{
    "agents": [
        {
            "agent_id": "com.example.weather",
            "name": "Weather Agent",
            "version": "1.0.0",
            "status": "running",       // running | stopped | error
            "dev": false,
            "pid": 12345               // running 时有值
        }
    ]
}

// POST /api/agents/install
// Request: { "path": "/path/to/weather.agent" }
// → 200 { "agent_id": "com.example.weather", "version": "1.0.0" }
// → 400 { "error": "invalid package" }
// → 409 { "error": "already installed" }

// POST /api/agents/:id/clone
// Request: { "mode": "skeleton" | "full", "new_id": "com.example.weather-dev" }
// → 200 { "agent_id": "com.example.weather-dev", "workspace": "/path/to/workspace" }
// → 400 { "error": "cannot clone system agent" }
```

#### 9.4.2 对话

```json
// POST /api/agents/:id/message
// Request: { "content": "北京今天天气怎么样" }
// → 200 { "message_id": "msg-001", "status": "queued" }
// → 404 { "error": "agent not found" }
// → 503 { "error": "agent not running" }

// GET /api/agents/:id/stream (WebSocket Upgrade)
// WebSocket 消息格式：
// → Client sends: { "type": "message", "content": "..." }
// ← Server pushes: { "type": "chunk", "delta": "今", "message_id": "msg-001" }
// ← Server pushes: { "type": "chunk", "delta": "天", "message_id": "msg-001" }
// ← Server pushes: { "type": "tool_call", "name": "http_request", "params": {...} }
// ← Server pushes: { "type": "tool_result", "name": "http_request", "result": {...} }
// ← Server pushes: { "type": "done", "message_id": "msg-001", "usage": {...} }
```

#### 9.4.3 Vault

```json
// GET /api/vault/keys
// → 200
{
    "keys": [
        { "provider": "openai", "has_key": true, "key_preview": "sk-...abc" },
        { "provider": "anthropic", "has_key": false }
    ]
}

// POST /api/vault/keys
// Request: { "provider": "openai", "key": "sk-proj-..." }
// → 201 { "provider": "openai" }
// → 400 { "error": "invalid key format" }
```

Vault 的 HTTP API **不返回明文 Key**，只返回存在性和脱敏预览（前 3 字符 + `...` + 后 3 字符）。

#### 9.4.4 系统状态

```json
// GET /api/status
// → 200
{
    "gateway_version": "0.1.0",
    "uptime_seconds": 3600,
    "agents_running": 3,
    "agents_total": 7,
    "memory_usage_mb": 128
}

// GET /health
// → 200 { "status": "ok" }
```

### 9.5 HTTP API 与 Socket API 的关系

HTTP API 中的 Agent 管理操作（安装/卸载/启停）直接调用 Gateway 内部组件，与 Socket API 的处理逻辑共享：

```
POST /api/agents/:id/start
       │
       ▼
Gateway::lifecycle_manager().start_agent("com.example.weather")
       │
       ▼
（与 Agent Runtime 通过 Socket 发起的启动请求走同一条代码路径）
```

对话消息的转发路径：

```
Desktop App → POST /api/agents/:id/message
       │
       ▼
Gateway → Intent Router → 转发给 Agent Runtime（通过 Socket API）
       │
       ▼
Agent Runtime 处理 → 响应通过 Gateway → Desktop App（WebSocket 推送）
```

HTTP API 不是独立于 Socket API 的旁路，而是 Socket API 的**管理面封装**。

### 9.6 安全设计

| 措施 | 说明 |
|------|------|
| 仅监听 localhost | 默认 `127.0.0.1`，不对外暴露 |
| Vault Key 脱敏 | GET 接口不返回明文，POST 接口接收明文 |
| 无 CORS | 生产环境不开启跨域（localhost only 天然限制） |
| 可选 Auth Token | Gateway 生成随机 token，Desktop App 首次连接时获取，后续请求携带 `Authorization: Bearer <token>` |
| Agent 安装校验 | 与 Socket API 一样强制验证包签名 |

Auth Token 机制（可选，Phase 5+）：
```
Gateway 启动时生成随机 token → 写入 ~/.config/agent-gateway/http_token
Desktop App 首次连接时读取该文件 → 后续请求携带
```

### 9.7 Desktop App 发现 Gateway

Desktop App 需要自动发现 Gateway 的 HTTP API 端口：

```rust
// 发现策略（按优先级）：
// 1. 读取 Desktop App 自身配置中保存的地址
// 2. 读取 Gateway 的 pidfile：~/.local/share/agent-gateway/gateway.pid
//    pidfile 内容：{ "pid": 12345, "http_port": 19876, "socket_path": "..." }
// 3. 尝试默认地址 http://127.0.0.1:19876/health
// 4. 提示用户手动配置
```

## 10. Gateway CLI

Gateway CLI 是一个独立的二进制（`rollball`），为无 GUI 场景提供命令行管理入口。CLI 通过 Gateway HTTP API 与 Gateway 通信，不直接操作 Gateway 内部状态。

### 10.1 命令接口

```bash
# Agent 管理
rollball install <path>          # 安装 .agent 包（含签名验证）
rollball uninstall <agent_id>    # 卸载 Agent
rollball list                    # 列出已安装 Agent 及状态
rollball start <agent_id>       # 启动 Agent
rollball stop <agent_id>        # 停止 Agent
rollball info <agent_id>        # 查看 Agent 详细信息（版本、权限、运行状态）

# 对话
rollball chat <agent_id>        # 进入交互式对话模式
rollball send <agent_id> <msg>  # 发送单条消息并输出响应

# Vault 管理
rollball vault list              # 列出已配置的 Key（脱敏显示）
rollball vault add <provider>    # 添加 API Key（交互式输入，不回显）
rollball vault remove <provider> # 移除 API Key

# 系统管理
rollball status                  # Gateway 状态（版本、运行 Agent 数、内存占用）
rollball config get [key]        # 查看 Gateway 配置
rollball config set <key> <val>  # 修改 Gateway 配置
```

### 10.2 与 Gateway 的通信

CLI 通过 Gateway HTTP API（`http://127.0.0.1:19876`）通信，与 Desktop App 共享同一套 REST 接口（见 §9）。发现机制也与 Desktop App 一致：pidfile → 默认端口 → 手动配置。

```
rollball CLI → HTTP API → Gateway
                    ↑
Desktop App 也走这条路径
```

### 10.3 设计决策

| 决策 | 选择 | 理由 |
|------|------|------|
| CLI 与 Gateway 独立 | 独立二进制 | Gateway 可无 CLI 运行（Desktop App 足够），CLI 可安装在不同机器远程管理（未来） |
| 通信方式 | HTTP API（非 Socket API） | Socket API 是二进制帧协议，面向 Agent Runtime；HTTP API 是标准 REST，CLI 天然适配 |
| API Key 输入 | 交互式不回显 | 防止 shell history 泄露 Key |
| CLI 框架 | clap | Rust 生态标准选择，与 rollball-sign 工具链保持一致 |

## 11. 设计决策记录

| 决策 | 选择 | 理由 |
|------|------|------|
| Gateway 不代理业务逻辑 | 纯协调层 | 避免单点瓶颈；Agent Runtime 直连 LLM 延迟更低 |
| 双 API 层 | Socket + HTTP | Socket API 面向 Agent Runtime IPC（高性能二进制帧），HTTP API 面向 Desktop App/CLI（标准 REST） |
| HTTP 框架 | Axum | Rust 生态最成熟的 HTTP 框架；Gateway 已在技术选型中确认 |
| HTTP 端口 | 127.0.0.1:19876 | 仅 localhost，安全；端口可配置；冲突时自动递增 |
| Vault HTTP 脱敏 | 不返回明文 | 防止 Desktop App 前端漏洞导致 Key 泄露；POST 接口接收明文即可 |
| Gateway 发现机制 | pidfile + 默认地址 | 简单可靠；pidfile 由 Gateway 启动时写入；Desktop App 按优先级尝试 |
| Desktop App 与 Gateway 独立 | 独立进程 | 与 opencode/openclaw/zeroclaw 一致；Gateway 可独立运行支持 CLI-only 用户 |
