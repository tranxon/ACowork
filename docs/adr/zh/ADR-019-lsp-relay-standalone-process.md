# ADR-019: LSP Relay 从 Gateway 解耦为独立进程

**状态**：草案（待决策）
**日期**：2026-07-01
**决策者**：架构讨论
**影响范围**：

**Phase 0 — 公共模块抽取（acowork-core 扩展）：**
- `core/acowork-core/src/event_bus.rs`（**新增**，从 `acowork-embed/src/event_bus.rs` 泛化）
- `core/acowork-core/src/shutdown.rs`（**新增**，从 `acowork-embed/src/shutdown.rs` 移入）
- `core/acowork-core/src/supervisor.rs`（**新增**，从 `embed_supervisor.rs` 抽取通用构建块）
- `core/acowork-core/src/health.rs`（**新增**，定义 `/health` + `/events` 端点契约）
- `core/acowork-embed/src/event_bus.rs`（**改为** `type EmbedEventBus = EventBus<EmbedState>`）
- `core/acowork-embed/src/shutdown.rs`（**删除**，改为 `use acowork_core::shutdown`）
- `core/acowork-gateway/src/lifecycle/embed_supervisor.rs`（**重构**，使用 `acowork_core::supervisor` 构建块）

**Phase 1~3 — LSP Relay 独立进程：**
- `core/acowork-gateway/src/lsp/mod.rs`（1752 行，**整体移出**）
- `core/acowork-gateway/src/lsp/pool.rs`（352 行，**整体移出**）
- `core/acowork-gateway/src/http/routes.rs`（删除 5 条 LSP 路由 + `AppState.lsp_pool` 字段）
- `core/acowork-gateway/src/http/server.rs`（删除 `start_reaper` 调用）
- `core/acowork-gateway/src/config.rs`（删除 `lsp_config_dir` 字段）
- `core/acowork-gateway/src/cli.rs`（删除 `--lsp-config-dir` 参数）
- `core/acowork-gateway/src/gateway/mod.rs`（新增 LSP Relay supervisor 启动逻辑）
- `core/acowork-gateway/src/gateway/state.rs`（新增 `lsp_relay_process` 状态字段）
- `core/acowork-lsp-relay/`（**新增 crate**，承载移出的 LSP 逻辑）
- `apps/acowork-desktop/src-tauri/`（Monaco 直连 LSP Relay，不再经过 Gateway）
- `core/acowork-runtime/`（codebase tool 直连 LSP Relay）

---

## 背景

### 问题 1：LSP 模块与 Gateway 耦合过重

Gateway 设计文档（`docs/design/zh/04-gateway.md`）明确其定位：

> Gateway **不代理 Agent 的业务逻辑**（不代理 LLM 调用、不代理工具执行），只负责必须集中化的协调工作。

Gateway 的核心职责是：Package Manager、Lifecycle Manager、Intent Router、Key Vault、Budget Tracker、Rate Limiter。这些职责的共同特征是**全局资源管理与协调**，不涉及具体业务协议。

然而，当前 LSP 模块（`core/acowork-gateway/src/lsp/`，共 2104 行，占 Gateway 代码量的 6.5%）承担了大量与 Gateway 定位不符的职责：

| 职责 | 代码量 | 与 Gateway 定位的冲突 |
|------|--------|----------------------|
| LSP 进程池管理（spawn/reap/idle timeout） | ~350 行 | 实质上是第二个 LifecycleManager |
| WebSocket ↔ stdin/stdout 双向 relay | ~400 行 | 协议代理，非透传 |
| LSP 协议状态管理（initialize 握手缓存、JSON-RPC id 替换） | ~200 行 | 深度解析和修改业务协议消息 |
| 安装脚本执行（15 分钟超时） | ~200 行 | 在 Gateway tokio runtime 上执行重型外部脚本 |
| 命令可运行性验证（两阶段探测） | ~150 行 | 业务逻辑 |
| 配置管理（lsp_servers.json 加载、内置默认） | ~300 行 | 业务配置 |
| HTTP API（5 条路由） | ~200 行 | 业务接口 |

### 问题 2：稳定性风险已有实证

LSP install 脚本执行曾阻塞 Gateway 的 tokio runtime，导致 embed 看门狗饿死，误杀 embed 进程（参见 `docs/plan/embed-heartbeat-timeout-fix.md`）。根本原因是 LSP 安装脚本可能执行 `npm install`、`pip install`、`cargo install` 等重型操作，这些操作的不确定性与 Gateway 的高稳定性要求矛盾。

### 问题 3：未来 codebase 工具对 LSP 有强依赖

编程 Agent 的 codebase 工具需要调用 LSP 协议获取代码智能：

```
Agent Runtime (codebase tool)
    │
    ├── textDocument/definition     → 需要 LSP
    ├── textDocument/references     → 需要 LSP
    ├── textDocument/hover          → 需要 LSP
    ├── workspace/symbol            → 需要 LSP
    └── textDocument/diagnostic     → 需要 LSP
```

如果 LSP 放在 Desktop App 前端，Agent Runtime 每次调用 codebase 工具都要经过前端中转——这在架构上不可接受：Agent Runtime 是后端进程，不能依赖前端是否打开。LSP 必须是**后端共享基础设施**。

### 问题 4：Gateway 对 IPC 是真透传，对 LSP 却深度解析

Gateway 对 Agent Runtime 的 gRPC IPC 是真正的协议透传——不解析消息内容，只做路由。但 LSP relay 却深度解析和修改 JSON-RPC 消息：

- `is_initialize_request` / `is_initialized_notification` / `is_initialize_result` — LSP 协议状态机
- `substitute_jsonrpc_id` — JSON-RPC id 替换
- `extract_method_hint` — LSP method 字段解析
- Initialize 握手缓存（`init_result: Mutex<Option<String>>`）

这种不一致性破坏了 Gateway 的架构简洁性。

## 目标

1. Gateway 彻底退出 LSP 数据路径，不再 proxy WebSocket、不再管理 LSP 进程池、不再解析 JSON-RPC
2. LSP 功能作为独立进程运行，Gateway 仅负责 spawn / monitor / restart（复用 embed supervisor 模式）
3. Desktop App（Monaco）和 Agent Runtime（codebase tool）直连 LSP Relay，不经过 Gateway 中转
4. LSP Relay 崩溃不影响 Gateway 稳定性，Gateway 崩溃后 LSP Relay 能自动退出

## 可选方案

### 方案 A：独立进程（acowork-lsp-relay）— 推荐

**原理**：将 LSP 模块整体移出 Gateway，作为独立二进制 `acowork-lsp-relay`。Gateway 通过 supervisor 模式管理其生命周期（spawn / monitor / restart），与 embed 的管理方式完全一致。

```
┌──────────────────────────────────────────────────────────┐
│                    Gateway (精简后)                       │
│                                                          │
│  ┌─────────────┐  ┌──────────┐  ┌────────────────────┐  │
│  │ Package Mgr │  │Lifecycle │  │ LSP Relay          │  │
│  │             │  │ Manager  │  │ Supervisor          │  │
│  ├─────────────┤  ├──────────┤  │ spawn/monitor/      │  │
│  │ Key Vault   │  │ Intent   │  │ restart             │  │
│  │             │  │ Router   │  └─────────┬──────────┘  │
│  ├─────────────┤  ├──────────┤            │              │
│  │ Budget      │  │ Rate     │   GET /api/lsp/endpoint  │
│  │ Tracker     │  │ Limiter  │   返回 LSP Relay 地址     │
│  └─────────────┘  └──────────┘                          │
└──────────────────────────────────────────────────────────┘
                    │ spawn + SSE heartbeat
                    ▼
┌──────────────────────────────────────────────────────────┐
│              acowork-lsp-relay (独立进程)                 │
│                                                          │
│  ┌──────────────────┐  ┌──────────────────────────────┐  │
│  │ WebSocket Server │  │ JSON-RPC API                 │  │
│  │ /lsp/:language   │  │ /api/codebase/*              │  │
│  │ (Monaco 直连)    │  │ (Agent Runtime codebase tool)│  │
│  ├──────────────────┤  ├──────────────────────────────┤  │
│  │ LSP Process Pool │  │ Install Scripts + Status     │  │
│  │ (spawn/reap)     │  │ /api/lsp/install/*           │  │
│  ├──────────────────┤  ├──────────────────────────────┤  │
│  │ Config           │  │ Health                       │  │
│  │ lsp_servers.json │  │ /health + SSE /events        │  │
│  └──────────────────┘  └──────────────────────────────┘  │
└──────────────────────────────────────────────────────────┘
        ▲                           ▲
        │ WebSocket 直连            │ JSON-RPC 直连
        │                           │
┌───────┴────────┐          ┌───────┴────────┐
│  Desktop App   │          │  Agent Runtime  │
│  (Monaco)      │          │  (codebase tool)│
└────────────────┘          └────────────────┘
```

**优点**：
- 完全隔离：LSP 崩溃不影响 Gateway，Gateway 崩溃后 LSP Relay 通过 supervisor 超时自退出
- 复用成熟模式：embed supervisor 已解决进程发现、健康检查、崩溃恢复、PID-aware reaper、startup grace window 等所有难题
- Gateway 大幅精简：删除 2104 行 LSP 代码 + 5 条路由 + AppState 字段 + Config 字段 + CLI 参数
- 客户端直连：Monaco 和 codebase tool 直连 LSP Relay，Gateway 不在数据路径上，零性能开销
- 独立扩展：LSP Relay 可独立升级、独立配置资源限制、独立选择 tokio runtime 参数

**缺点**：
- 新增一个 crate 和一个二进制，增加构建复杂度
- Desktop App 需要先查询 Gateway 获取 LSP Relay 端口，再直连（多一次 HTTP 请求）
- 需要协调三个组件的版本发布

### 方案 B：独立 crate，仍在 Gateway 进程内

**原理**：将 LSP 逻辑抽离为 `acowork-lsp-relay` crate，但仍在 Gateway 进程中以 library 形式运行。

**优点**：
- 代码隔离，不新增进程
- 改动量较小

**缺点**：
- 不解决稳定性隔离问题：LSP 仍在 Gateway 的 tokio runtime 上运行
- 不解决资源竞争问题：LSP 进程池仍消耗 Gateway 的内存和 CPU
- 不解决安装脚本阻塞问题
- 本质上只是代码重组，不是架构解耦

### 方案 C：移到 Desktop App（Tauri 侧）

**原理**：LSP relay 作为 Tauri sidecar 或内嵌服务运行。

**优点**：
- Gateway 完全不受影响

**缺点**：
- Agent Runtime 的 codebase tool 无法使用 LSP（前端不一定打开）
- LSP 进程生命周期绑定到 Desktop App，而非系统服务
- 与未来编程 Agent 的架构方向矛盾

## 决策

推荐采用 **方案 A：独立进程（acowork-lsp-relay）**。

### 理由

1. **方案 B 不解决根本问题**：代码隔离不等于运行时隔离，LSP 安装脚本仍可能阻塞 Gateway runtime
2. **方案 C 与未来方向矛盾**：codebase 工具需要 LSP 作为后端基础设施，不能依赖前端
3. **方案 A 复用已验证模式**：embed supervisor 已经验证了"独立进程 + Gateway supervisor"的可行性，LSP Relay 可以直接复用同一套模式
4. **方案 A 是唯一同时满足"Gateway 稳定"和"codebase 可用"的方案**

## 前置工作：公共模块抽取（Phase 0，高优先级）

在创建 `acowork-lsp-relay` 之前，需要先将 embed 和 LSP relay 的公共模式抽取到 `acowork-core`，避免代码重复。embed 和 LSP relay 作为"Gateway 管理的独立子进程"，共享以下基础设施需求：

### 公共模式分析

| # | 模块 | 当前所在 | 代码量 | 复用方 | 复用价值 |
|---|------|----------|--------|--------|----------|
| 1 | **EventBus** — broadcast channel + heartbeat + SSE 事件模型 | `acowork-embed/src/event_bus.rs` | 113 行 | embed, LSP relay | **高** — 两者都需要 SSE heartbeat 供 supervisor 监控 |
| 2 | **Shutdown** — 跨平台信号处理（SIGTERM/SIGINT/Ctrl+C） | `acowork-embed/src/shutdown.rs` | 77 行 | embed, LSP relay | **高** — 两者都需要优雅退出 |
| 3 | **Supervisor 构建块** — RestartHistory、指数退避、SSE 帧解析、heartbeat watchdog | `embed_supervisor.rs` | ~400 行 | embed supervisor, LSP relay supervisor | **高** — 两者 supervisor 逻辑几乎相同 |
| 4 | **Health 端点契约** — `/health` 和 `/events` 的响应格式约定 | 隐式约定 | — | embed, LSP relay | **中** — 统一契约减少 supervisor 差异 |
| 5 | **Idle timeout** — 子进程无输出超时 | `acowork-core/src/process.rs` | 已有 | LSP install, embed download | **已有** — 直接复用，无需改动 |

### 抽取设计

#### 1. `acowork-core::event_bus` — 通用事件总线（新增）

从 `acowork-embed/src/event_bus.rs` 泛化，将 embed 特定的 `State` 枚举替换为泛型参数：

```rust
// core/acowork-core/src/event_bus.rs

use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::broadcast;

/// Generic event flowing over the bus.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BusEvent<S: Clone + Serialize> {
    /// Periodic liveness signal.
    Heartbeat { seq: u64 },
    /// Application-level state transition.
    State { seq: u64, state: S },
}

/// Bus for broadcasting events to all subscribers.
///
/// Uses `tokio::sync::broadcast` internally. Each new subscriber starts
/// receiving events from the moment of subscription onwards.
///
/// # Type parameter
///
/// `S` is the application-specific state type. For embed it's `EmbedState`
/// (Starting, Loading, Ready, Error); for LSP relay it's `LspRelayState`
/// (Starting, Ready, Error).
#[derive(Clone)]
pub struct EventBus<S: Clone + Serialize + Send + Sync + 'static> {
    tx: broadcast::Sender<Arc<BusEvent<S>>>,
    seq: Arc<AtomicU64>,
}

impl<S: Clone + Serialize + Send + Sync + 'static> EventBus<S> {
    pub fn new(buffer: usize) -> Self { /* ... */ }
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<BusEvent<S>>> { /* ... */ }
    pub fn publish_state(&self, state: S) -> u64 { /* ... */ }
    pub fn spawn_heartbeat(&self, interval_ms: u64) { /* ... */ }
}
```

**embed 适配**：`type EmbedEventBus = EventBus<embed::State>;`

**LSP relay 适配**：`type LspRelayEventBus = EventBus<LspRelayState>;`

#### 2. `acowork-core::shutdown` — 通用优雅退出（新增）

从 `acowork-embed/src/shutdown.rs` 移入，无需泛化（逻辑完全通用）：

```rust
// core/acowork-core/src/shutdown.rs

pub struct Shutdown { flag: AtomicBool }
impl Shutdown {
    pub fn new() -> Arc<Self> { /* ... */ }
    pub fn is_shutting_down(&self) -> bool { /* ... */ }
    pub fn request(&self) { /* ... */ }
}
pub fn install_signal_handlers(shutdown: Arc<Shutdown>) { /* ... */ }
```

#### 3. `acowork-core::supervisor` — Supervisor 构建块（新增）

从 `embed_supervisor.rs` 中抽取与 embed 业务无关的通用逻辑：

```rust
// core/acowork-core/src/supervisor.rs

/// Tracks consecutive restart attempts within a sliding window.
pub struct RestartHistory { /* ... */ }
impl RestartHistory {
    pub fn new() -> Self { /* ... */ }
    pub fn record(&mut self, window: Duration) -> usize { /* ... */ }
}

/// Compute exponential backoff with ±20% jitter, clamped to [min, max].
pub fn backoff_with_jitter(attempt: u32, min: Duration, max: Duration) -> Duration { /* ... */ }

/// Minimal SSE frame parser. Returns `None` for comments or unparseable frames.
pub fn parse_sse_frame(frame: &str) -> Option<SseFrame> { /* ... */ }

pub enum SseFrame {
    Heartbeat,
    State(String),  // raw JSON payload — caller deserializes
    Comment(String),
}

/// Heartbeat watchdog: wraps a tokio::time::Interval and checks elapsed
/// time since the last heartbeat. Returns `true` when timeout exceeded.
pub struct HeartbeatWatchdog {
    interval: tokio::time::Interval,
    last_heartbeat: Instant,
    timeout: Duration,
}
impl HeartbeatWatchdog {
    pub fn new(check_interval: Duration, timeout: Duration) -> Self { /* ... */ }
    /// Wait for the next tick, then check if heartbeat is stale.
    pub async fn tick(&mut self) -> HeartbeatStatus { /* ... */ }
    /// Call on every received heartbeat to reset the timer.
    pub fn beat(&mut self) { /* ... */ }
}

pub enum HeartbeatStatus {
    Ok,
    Timeout { elapsed_secs: u64 },
}
```

#### 4. `acowork-core::health` — Health/Events 端点契约（新增）

定义 Gateway supervisor 与被管理子进程之间的标准契约：

```rust
// core/acowork-core/src/health.rs

/// Standard health check response that every Gateway-managed subprocess
/// MUST return from `GET /health`.
#[derive(Debug, Serialize, Deserialize)]
pub struct HealthResponse {
    /// "ok" | "degraded" | "starting"
    pub status: String,
    /// Process version (from CARGO_PKG_VERSION)
    pub version: String,
    /// Process name for diagnostics (e.g. "acowork-embed", "acowork-lsp-relay")
    pub process: String,
    /// Process-specific payload (model info for embed, language count for LSP relay)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// Standard SSE event names used by the supervisor.
pub mod sse_event {
    pub const HEARTBEAT: &str = "heartbeat";
    pub const STATE: &str = "state";
}

/// Recommended constants for supervisor configuration.
pub mod supervisor_defaults {
    use std::time::Duration;
    pub const HEARTBEAT_INTERVAL_MS: u64 = 2000;
    pub const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);
    pub const STARTUP_GRACE: Duration = Duration::from_secs(10);
    pub const STARTUP_POLL: Duration = Duration::from_secs(2);
    pub const RESTART_BACKOFF_MIN: Duration = Duration::from_secs(1);
    pub const RESTART_BACKOFF_MAX: Duration = Duration::from_secs(60);
    pub const RESTART_WINDOW: Duration = Duration::from_secs(5 * 60);
    pub const MAX_RESTART_ATTEMPTS: u32 = 5;
}
```

### 抽取后的 crate 依赖关系

```
acowork-core (新增 event_bus, shutdown, supervisor, health)
    ▲                    ▲
    │                    │
    │                    │
acowork-embed      acowork-lsp-relay
(使用 EventBus<EmbedState>,    (使用 EventBus<LspRelayState>,
 Shutdown, /health 契约)        Shutdown, /health 契约)

acowork-gateway
(使用 supervisor 构建块: RestartHistory, backoff_with_jitter,
 HeartbeatWatchdog, parse_sse_frame, supervisor_defaults)
```

### 实施优先级

| 阶段 | 内容 | 优先级 | 理由 |
|------|------|--------|------|
| **Phase 0a** | `acowork-core::shutdown` | **P0** | 无依赖，77 行，embed 直接切换 |
| **Phase 0b** | `acowork-core::event_bus` | **P0** | 泛化后 embed 和 LSP relay 都可用 |
| **Phase 0c** | `acowork-core::health` | **P0** | 定义契约，后续实现有据可依 |
| **Phase 0d** | `acowork-core::supervisor` | **P1** | 依赖前三者，Gateway supervisor 重构用 |
| **Phase 1** | 创建 `acowork-lsp-relay` | P1 | 依赖 Phase 0 完成 |
| **Phase 2** | Gateway 集成 supervisor | P2 | 依赖 Phase 1 |
| **Phase 3** | 切换客户端 + 清理 | P3 | 依赖 Phase 2 |

### 详细设计

#### 1. 新增 crate：`core/acowork-lsp-relay/`

```
core/acowork-lsp-relay/
├── Cargo.toml
└── src/
    ├── main.rs              # 入口：解析 CLI、初始化 EventBus + Shutdown、启动 HTTP+WS server
    ├── lib.rs               # 库入口
    ├── state.rs             # LspRelayState 枚举（Starting, Ready, Error）
    ├── config.rs            # lsp_servers.json 加载（从 Gateway 移入）
    ├── pool.rs              # LSP 进程池（从 Gateway 移入，无改动）
    ├── relay.rs             # WebSocket ↔ stdin/stdout relay（从 Gateway 移入）
    ├── protocol.rs          # LSP 协议辅助（initialize 缓存、JSON-RPC id 替换）
    ├── install.rs           # 安装脚本管理（从 Gateway 移入，使用 acowork_core::process::run_command_with_idle_timeout）
    ├── codebase.rs          # JSON-RPC API for Agent Runtime codebase tool（新增）
    ├── server.rs            # Axum HTTP + WebSocket server（使用 acowork_core::event_bus::EventBus<LspRelayState>）
    └── health.rs            # /health（返回 acowork_core::health::HealthResponse）+ SSE /events
```

**依赖**：
- `acowork-core`（event_bus, shutdown, health, process, logging）
- `axum` + `tokio` + `serde_json` + `tracing`
- 不依赖 `acowork-gateway`

**CLI 参数**：

```rust
#[derive(Parser)]
struct Cli {
    /// HTTP 监听地址
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// HTTP 监听端口（0 = 自动分配）
    #[arg(long, default_value = "0")]
    port: u16,

    /// LSP 配置文件目录
    #[arg(long)]
    lsp_config_dir: Option<String>,

    /// Gateway health URL（用于自退出检测）
    #[arg(long)]
    gateway_health_url: Option<String>,

    /// Gateway 断连超时（毫秒）
    #[arg(long, default_value = "300000")]
    gateway_health_timeout_ms: u64,

    /// Gateway 健康探测间隔（毫秒）
    #[arg(long, default_value = "10000")]
    gateway_health_interval_ms: u64,
}
```

#### 2. Gateway 侧改动

**删除**：
- `core/acowork-gateway/src/lsp/mod.rs`（1752 行）
- `core/acowork-gateway/src/lsp/pool.rs`（352 行）
- `AppState.lsp_pool` 字段
- 5 条 LSP HTTP 路由（`/lsp/{language}`、`/api/lsp/servers`、`/api/lsp/status`、`/api/lsp/install/{language}` GET/POST）
- `server.rs` 中的 `LspPool::start_reaper` 调用
- `config.rs` 中的 `lsp_config_dir` 字段
- `cli.rs` 中的 `--lsp-config-dir` 参数

**新增**：

`core/acowork-gateway/src/lifecycle/lsp_relay.rs`（参考 `embed.rs`）：

```rust
/// LSP Relay 进程状态
#[derive(Debug, Clone)]
pub struct LspRelayProcessState {
    pub pid: u32,
    pub port: u16,
    pub ready: bool,
}

/// 启动 acowork-lsp-relay 进程
pub async fn spawn_lsp_relay(
    lsp_config_dir: Option<&str>,
    port: u16,
    gateway_health_url: &str,
) -> Result<(LspRelayProcessState, tokio::process::Child), GatewayError>;

/// 杀死 LSP Relay 进程
pub async fn kill_lsp_relay(pid: u32) -> Result<(), GatewayError>;
```

`core/acowork-gateway/src/lifecycle/lsp_relay_supervisor.rs`（参考 `embed_supervisor.rs`）：

```rust
/// 启动 LSP Relay supervisor
///
/// 监控 LSP Relay 的 SSE /events 流，检测心跳超时（10s），
/// 崩溃后指数退避重启（5次/5分钟上限）。
pub fn start_lsp_relay_supervisor(
    cfg: LspRelaySupervisorConfig,
    state: SharedState,
);
```

`core/acowork-gateway/src/gateway/state.rs`：

```rust
pub struct GatewayState {
    // ... 现有字段 ...
    /// LSP Relay 进程状态（None if not started）
    pub lsp_relay_process: Option<LspRelayProcessState>,
}
```

`core/acowork-gateway/src/http/routes.rs`：

```rust
/// GET /api/lsp/endpoint — 返回 LSP Relay 的地址
///
/// Desktop App 和 Agent Runtime 通过此端点发现 LSP Relay，
/// 然后直连 LSP Relay 的 WebSocket 和 JSON-RPC API。
pub async fn lsp_endpoint(State(state): State<AppState>) -> Json<LspEndpointResponse> {
    let gw = state.gateway_state.read().await;
    match &gw.lsp_relay_process {
        Some(eps) if eps.ready => Json(LspEndpointResponse {
            available: true,
            host: "127.0.0.1".to_string(),
            port: Some(eps.port),
        }),
        _ => Json(LspEndpointResponse {
            available: false,
            host: "127.0.0.1".to_string(),
            port: None,
        }),
    }
}
```

#### 3. Desktop App 侧改动

Monaco Editor 连接流程变更：

```
旧流程：
  Monaco → WebSocket ws://127.0.0.1:19876/lsp/rust
           (经过 Gateway 中转)

新流程：
  1. GET http://127.0.0.1:19876/api/lsp/endpoint → { port: 19878 }
  2. Monaco → WebSocket ws://127.0.0.1:19878/lsp/rust
              (直连 LSP Relay)
```

LSP 安装/状态 UI 同样改为直连 LSP Relay。

#### 4. Agent Runtime 侧改动

codebase tool 获取 LSP Relay 地址：

```
旧流程（不存在）：
  N/A

新流程：
  1. AgentHelloResult 中新增 lsp_relay_endpoint 字段
  2. codebase tool 通过 gRPC 获取 LSP Relay 地址
  3. codebase tool → JSON-RPC http://127.0.0.1:{port}/api/codebase/definition
```

#### 5. 生命周期管理

```
Gateway 启动:
  1. spawn acowork-lsp-relay（端口自动分配或固定）
  2. 等待 LSP Relay /health 就绪（startup grace 10s）
  3. 启动 LSP Relay supervisor（SSE heartbeat 监控）
  4. 将 LSP Relay 状态写入 GatewayState.lsp_relay_process

Gateway 正常运行:
  - supervisor 监控 SSE heartbeat（2s 间隔，10s 超时）
  - 崩溃 → 指数退避重启（1s, 2s, 4s, 8s, ... 最大 60s）
  - 5 次/5 分钟上限 → 放弃，标记 unavailable

Gateway 正常退出:
  1. kill LSP Relay 进程
  2. 等待子进程退出
  3. Gateway 自身退出

Gateway 异常崩溃:
  - LSP Relay 检测 gateway_health_url 不可达
  - 300s 超时后自动 exit(0)（复用 ADR-018 模式）
```

#### 6. 端口分配策略

| 策略 | 说明 |
|------|------|
| 默认 | `--port 0`，OS 自动分配，实际端口通过 `/api/lsp/endpoint` 返回 |
| 固定 | `--port 19878`，用于调试和固定部署 |
| 冲突处理 | 与 Gateway HTTP server 相同的端口递增策略 |

### 与 embed supervisor 模式的对比

| 维度 | embed | LSP Relay |
|------|-------|-----------|
| 独立二进制 | `acowork-embed` | `acowork-lsp-relay` |
| Gateway 职责 | spawn / monitor / restart | spawn / monitor / restart |
| 健康监控 | SSE heartbeat（2s 间隔，10s 超时） | SSE heartbeat（2s 间隔，10s 超时） |
| 崩溃恢复 | 指数退避，5次/5分钟上限 | 指数退避，5次/5分钟上限 |
| 状态存储 | `GatewayState.embed_process` | `GatewayState.lsp_relay_process` |
| 端口发现 | 固定 18080 | 自动分配或固定 |
| 关闭 | Gateway shutdown → kill child | Gateway shutdown → kill child |
| 自退出 | ADR-018: Gateway health probe, 300s 超时 | ADR-018: Gateway health probe, 300s 超时 |
| 可选性 | 不可用时 fallback 到远程 embedding | 不可用时 LSP 功能不可用（无 fallback） |

## 影响

**正向影响**：
- Gateway 代码量减少 ~2104 行（6.5%），复杂度显著降低
- LSP 崩溃不再影响 Gateway 稳定性
- LSP 安装脚本在独立进程中执行，不会阻塞 Gateway runtime
- Gateway 对 LSP 的职责从"协议代理 + 进程池管理 + 安装执行"简化为"spawn / monitor / restart"
- 为 codebase 工具提供直接可用的 LSP 后端基础设施
- LSP Relay 可独立升级、独立配置资源限制

**负面影响**：
- 新增一个 crate 和一个二进制，构建时间增加
- Desktop App 多一次 HTTP 请求（获取 LSP Relay 端口）
- 需要协调三个组件（Gateway、LSP Relay、Desktop App）的版本发布
- LSP Relay 不可用时无 fallback（与 embed 可 fallback 到远程不同）

**缓解措施**：
- LSP Relay 端口可固定配置，Desktop App 可缓存端口避免每次查询
- LSP Relay 作为可选组件：如果二进制不存在，Gateway 跳过启动，LSP 功能不可用但不影响其他功能
- 版本协调通过 Gateway 的 `/api/lsp/endpoint` 响应中包含版本号，Desktop App 可做兼容性检查

## 迁移路径

建议分四个阶段实施，Phase 0（公共模块抽取）为最高优先级，优先于 LSP relay 的创建。

### Phase 0：公共模块抽取到 acowork-core（P0，先行）

1. **Phase 0a**：`acowork-core::shutdown` — 从 `acowork-embed/src/shutdown.rs` 移入，embed 改为 `use acowork_core::shutdown`
2. **Phase 0b**：`acowork-core::event_bus` — 泛化 `EventBus<S>`，embed 改为 `type EmbedEventBus = EventBus<EmbedState>`
3. **Phase 0c**：`acowork-core::health` — 定义 `HealthResponse`、SSE 事件名常量、supervisor 默认参数
4. **Phase 0d**：`acowork-core::supervisor` — 抽取 `RestartHistory`、`backoff_with_jitter`、`HeartbeatWatchdog`、`parse_sse_frame`，Gateway 的 `embed_supervisor.rs` 改为使用这些构建块

### Phase 1：创建 acowork-lsp-relay crate（无破坏性）

1. 创建 `core/acowork-lsp-relay/` crate，依赖 `acowork-core`（使用 event_bus, shutdown, health）
2. 将 `lsp/mod.rs` 和 `lsp/pool.rs` 的代码移入新 crate
3. 添加 `main.rs`、`server.rs`、`health.rs`（使用 `acowork_core::event_bus::EventBus<LspRelayState>`）
4. Gateway 中保留原有 LSP 模块不变（双轨运行）

### Phase 2：Gateway 集成 supervisor

1. Gateway 新增 `lifecycle/lsp_relay.rs` 和 `lifecycle/lsp_relay_supervisor.rs`（使用 `acowork_core::supervisor` 构建块）
2. Gateway 启动时 spawn LSP Relay 进程
3. 新增 `GET /api/lsp/endpoint` 端点
4. 验证 LSP Relay 功能与原有 Gateway 内 LSP 功能等价

### Phase 3：切换客户端 + 清理 Gateway

1. Desktop App Monaco 改为直连 LSP Relay
2. Agent Runtime codebase tool 接入 LSP Relay
3. 删除 Gateway 中的 `lsp/` 模块、LSP 路由、AppState.lsp_pool、Config/Cli 中的 LSP 字段
4. 清理 `lsp_servers.json` 和 `lsp_install/` 的加载路径（移入 LSP Relay）

## 未解决的问题

- LSP Relay 是否需要支持多实例？（当前设计为单实例，与 embed 一致）
- LSP Relay 的端口是否需要在 Gateway 配置文件中可配置？
- 如果用户已有独立运行的 LSP Relay（非 Gateway 管理），Gateway 如何发现和 attach？（参考 embed 的 `attach_existing_embed_process` 模式）
- LSP Relay 的日志是独立文件还是由 Gateway 统一收集？
