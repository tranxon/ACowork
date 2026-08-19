# ADR-031：废弃旧 IPC 通道残留 — 全面收敛到 gRPC

**状态**：待实施
**日期**：2026-07-09
**决策者**：大鱼
**前置**：ADR-016（IPC gRPC 迁移设计）

---

## 决策摘要

**6 个原子提交，每个独立可 build + 通过全测**：

| Commit | 范围 | 文件数 | LOC | 风险 |
|--------|------|--------|-----|------|
| **C1** | Gateway `ipc/server.rs` → `handlers/` 目录（纯模块重命名） | ~16 | +120 / -120 | 低 |
| **C2** | 合并 `SessionManager` + `GrpcSessionManager` 为单一注册表 | ~8 | +250 / -400 | 中 |
| **C3** | Gateway `ipc/global_push.rs` → `grpc/resource_pusher.rs` | ~8 | +60 / -60 | 低 |
| **C4** | Runtime 删除整个 `pub mod ipc`（已空壳） | ~3 | +5 / -45 | 低 |
| **C5** | 删除 `socket_path` 配置 / 重命名 `gateway_socket` | ~8 | +30 / -50 | 低 |
| **C6** | proto 包名 `acowork.ipc.v1` → `acowork.gateway.v1` + 注释清理 | ~4 | +20 / -15 | 中（breaking） |

**关键决策**：

| 决策 | 理由 |
|------|------|
| 不做 deprecation window，直接删除 | 项目处于开发阶段，无外部依赖（ADR-016 §1.3 已明确原则） |
| `SessionManager` 合并到 `GrpcSessionManager`，不另起新类型 | 减少类型数量，`GrpcSession` 已功能完备 |
| `Session` 的 `pending_requests` / `next_id` 死字段直接删 | 生产代码 0 引用（仅测试中出现） |
| proto 包名一次性改，不保留双包名 | 根除 `ipc` 命名残留，`acowork.gateway.v1` 语义准确 |
| CLI `--gateway-socket` 保留为 deprecated alias | 避免破坏现有 runtime 命令行调用方（如系统脚本） |

---

## 背景

### 现状

gRPC 传输层切换（ADR-016）已 100% 完成：
- Gateway **只监听** TCP `127.0.0.1:19877` 的 tonic gRPC server
- Runtime **只发起** `GatewayGrpcClient::connect()` 连接上述 gRPC endpoint
- 没有 `TcpListener` / `UnixListener` / `UnixStream` 处理旧的 5 字节定长头 + JSON body 协议

但残留了**模块命名误导**和**架构层冗余**：

#### 残留 1：`ipc/server.rs` 命名欺骗 — 1385 行

位置：`core/acowork-gateway/src/ipc/server.rs`

文件名含 `server.rs`，但文件内**没有任何 server 代码**（无 bind、无 accept、无 connect、无 frame 读写）。它是一个纯业务逻辑函数集合：

```rust
handle_key_release()      // 14 个 handler 函数
handle_intent_send()      // 全部被 grpc/dispatch.rs 唯一引用
handle_budget_query()
handle_usage_report()
handle_rate_acquire()
handle_capability_query()
handle_cron_register()
handle_cron_unregister()
handle_cron_list()
handle_context_usage_report()
handle_agent_hello()
handle_agent_ready()
// + ResolvedLlmConfig / resolve_llm_config_for_agent()
```

注释也过期：
```rust
//! handlers are shared between the gRPC server (grpc/dispatch.rs)
//! and can be used by any transport layer.    // ← 不存在"any transport"了
```

#### 残留 2：双 SessionManager — 架构冗余 ~150 行

位置：`core/acowork-gateway/src/grpc/server.rs:454-464`

```rust
pub struct GatewayGrpcService {
    grpc_session_mgr: SharedGrpcSessionMgr,  // ✅ gRPC 专属, GrpcSession
    ipc_session_mgr: SharedSessionMgr,       // ❌ 老 Session, 每个连接双重注册
}
```

每个 gRPC 连接**同时注册到两个管理器**（`server.rs:488-501`）：
```rust
// 注册到 GrpcSessionManager
mgr.create_session(&conn_id, outbound_tx.clone());
// 同时注册到旧 SessionManager（用于兼容 handler）
mgr.create_session_with_push(&conn_id, ipc_push_tx);
```

这样做的唯一原因：dispatch handler 通过 `conn_id` 查 `SessionManager` 拿 `agent_id`，而 `GrpcSessionManager` 也具备完全相同的查询能力。

`Session` 类型中 `pending_requests` / `next_id` / `push_tx` / `push_message` 在生产代码中**均为 0 引用**（仅自身测试代码引用）。

#### 残留 3：`ipc/global_push.rs` 位置错误 — 475 行

`GlobalResourcePusher` **只通过 `SharedGrpcSessionMgr` 推送**：

```rust
use crate::grpc::SharedGrpcSessionMgr;     // ← 依赖的是 gRPC 模块
pub struct GlobalResourcePusher {
    grpc_session_mgr: Option<SharedGrpcSessionMgr>,  // ← 字段也是 gRPC
    ...
}
```

放在 `ipc/` 下纯属历史原因。

#### 残留 4：Runtime `pub mod ipc` 空壳 — 37 行

`runtime/src/ipc/client.rs` — `LlmConfigReceived` 结构体，已在 `AgentHelloConfig`（gRPC client）中完整替代，runtime 内**0 引用**。但 `lib.rs:15` 仍然 `pub mod ipc;` 暴露它。

#### 残留 5：CLI/Config 命名误导

| 位置 | 字段名 | 实际语义 | 问题 |
|------|--------|---------|------|
| `gateway/config.rs:61` | `socket_path` | **未使用** | 默认 `gateway.sock`，但无任何代码对此文件做 bind/remove |
| `gateway/cli.rs:54` | `--socket-path` | 同上 | 参数传递但无消费方 |
| `runtime/config.rs:52` | `gateway_socket` | gRPC URL | 名字误导 |
| `runtime/cli.rs:56-57` | `--gateway-socket` / `ACOWORK_GATEWAY_SOCKET` | gRPC URL | 名字误导 |
| `runtime/startup/context.rs:82` | `socket_path` | gRPC URL | 名字误导，且传到 `run_gateway_loop` 后立即 `_socket_path` 丢弃 |

#### 残留 6：proto 包名 `acowork.ipc.v1` + 注释

- proto 文件包名 `acowork.ipc.v1` — 已改为 `acowork.gateway.v1`
- 全 workspace 清理了 ~20 处注释中残留的 "IPC" 字眼（Gateway + Runtime 双端）
- 日志字符串 `"IPC session manager"` → `"gRPC session manager"` 等同步更新

---

## 实施计划

### Commit C1：Gateway `ipc/server.rs` → `handlers/` 模块重命名

**操作**：
1. `git mv core/acowork-gateway/src/ipc/server.rs` → `core/acowork-gateway/src/handlers.rs`
2. `git mv core/acowork-gateway/src/ipc/session.rs` → `core/acowork-gateway/src/handlers/session_state.rs`
3. 新建 `core/acowork-gateway/src/handlers/mod.rs`：
   ```rust
   //! Gateway handler functions for Gateway Service API requests.
   //! These handlers are invoked by grpc/dispatch.rs after proto decoding.
   pub mod session_state;
   pub use session_state::{Session, SessionManager};
   pub use super::*;
   // (re-export 公共类型：SharedState, SharedSessionMgr, handle_*, ResolvedLlmConfig)
   ```
   _注：具体 re-export 模式参考 C2 合并后的结果_
4. 创建 `core/acowork-gateway/src/ipc/mod.rs` 的**新替代**（或直接在 C1 删除 `pub mod ipc;` 中的 namespace 改为 `pub mod handlers;`）
5. 更新 `gateway/lib.rs:16`：`pub mod ipc;` → `pub mod handlers;`（如果 C1 直接移除了 pub mod ipc 中的 server/session）
6. 更新所有 `use crate::ipc::server::{...}` → `use crate::handlers::{...}`

**参考：涉及的文件（16 个）**：

| 文件 | 旧 import | 新 import |
|------|-----------|-----------|
| `grpc/dispatch.rs:15-21` | `ipc::server::*` + `ipc::session::SessionManager` | `handlers::*` + `handlers::session_state::SessionManager` |
| `grpc/server.rs:25` | `ipc::server::SharedState` | `handlers::SharedState` |
| `intent/router.rs:19-20` | `ipc::server::SharedState` + `ipc::session::SessionManager` | `handlers::*` |
| `http/routes.rs:22` | `ipc::session::SessionManager` | `handlers::session_state::SessionManager` |
| `http/server.rs:18` | `ipc::global_push::GlobalResourcePusher` | 不变（C3 再改） |
| `lifecycle/lsp_relay_supervisor.rs:26` | 同上 | 不变（C3 再改） |
| `lifecycle/embed_supervisor.rs:44` | 同上 | 不变（C3 再改） |
| `cron/mod.rs:21` | `ipc::session::SessionManager` | `handlers::session_state::SessionManager` |
| `gateway/mod.rs:16-17` | `ipc::server::SharedState` + `ipc::global_push::GlobalResourcePusher` | 部分不变（C3 再改） |

**C1 的关键**：不要大幅度改逻辑，只做 import 路径修正。C2 再动架构。

### Commit C2：合并双 SessionManager（核心工作）

**目标**：删除 `ipc_session_mgr: SharedSessionMgr` 双注册，所有 handler 直接使用 `GrpcSessionManager`。

**分析**：

`ipc::Session` 的生产活跃字段只有：
- `agent_id: Option<String>` — 也在 `GrpcSession.agent_id` 存在
- `connection_role: String` — 也在 `GrpcSession.connection_role` 存在

`GrpcSession` 比 `Session` 多：
- `push_tx: mpsc::Sender<Result<proto::ServerMessage, Status>>` — gRPC outbound
- 代理 push: `push_message(msg: GatewayResponse)` / `push_proto(msg: proto::ServerMessage)` / `push_request(msg: proto::ServerMessage)`
- `pending_requests` / `session_requests` / `next_request_id` — HTTP→Runtime 请求-响应模式

旧 `Session` 的 `push_message(msg: GatewayResponse)` 已通过 `GrpcSession::push_message` 实现（内部调用 `to_proto()` 转发到 gRPC outbound）。

**方案**：删除 `Session`、`SessionManager`，将所有 handler 使用的 `conn_id → agent_id` 查询改为 `GrpcSessionManager::get_session()`。

**步骤**：

1. **修改 handler 函数签名**（`handlers.rs` / `handlers/session_state.rs`）：
   - `handle_key_release(provider, conn_id, state, session_mgr)` → `handle_key_release(provider, conn_id, state)`
     - 内部 `session_mgr.get_session(conn_id).and_then(|s| s.agent_id.clone())`
     - 改为 `self.get_session(conn_id)...`（从成员方法访问），或
     - 由调用方（`dispatch.rs`）预先提取 `agent_id` 传入
   - 同理解析所有 14 个 handler，消除对 `SessionManager` 的依赖

   **推荐方式**：dispatch handler 统一提取 agent_id 后传参进入 handler，这样 handler 纯粹变成业务逻辑函数（与连接层完全解耦）。

2. **修改 `dispatch_grpc_request` 签名**：
   从：
   ```
   dispatch_grpc_request(client_msg, conn_id, state, session_mgr, bridge_ctrl_tx, session_pending)
   ```
   改为：
   ```
   dispatch_grpc_request(client_msg, conn_id, agent_id, state, bridge_ctrl_tx, session_pending)
   ```
   `dispatch_grpc_request` 内部调用 `agent_id` 替代 `session_mgr.get_session(conn_id).agent_id`。

3. **`GatewayGrpcService::connect`** 删除双注册：
   ```rust
   // 删除:
   let (ipc_push_tx, mut ipc_push_rx) = mpsc::channel(...);
   ipc_session_mgr.lock().await.create_session_with_push(&conn_id, ipc_push_tx);
   // 以及:
   ipc_session_mgr.lock().await.remove_session(&conn_id_clone);
   ```
   - 保留 `grpc_session_mgr.create_session(...)` 作为唯一注册

4. **`GatewayGrpcService` 结构体**删除 `ipc_session_mgr` 字段

5. **移除 `tokio::select!` 中的 Branch 2**（`ipc_push_rx.recv()` → 桥接推送分支）
   - 原来 Branch 2 消费 `ipc_push_rx` 并用 `msg.to_proto(0)` 转发到 gRPC outbound
   - 现在 `GrpcSession::push_message` 直接走 `self.push_tx.send(Ok(proto_msg))`，已经自带 `to_proto(0)` 转换
   - 所有向外 `push_message` 的路径（`http/agents.rs`、`http/embedding_api.rs`、`intent/router.rs`等）已经通过 `GrpcSessionManager::push_to_agent()` 或 `SharedGrpcSessionMgr` 调用
   - 这意味着 Branch 2 **已经是冗余路径**，可以安全删除

6. **删除 `Session` 中的死字段**（备选：与 C1 合并删除或 C2 一步到位）：
   - `Session::pending_requests: HashMap<u64, String>` — 0 引用
   - `Session::next_request_id: u64` — 仅测试引用
   - `Session::push_tx: Option<PushSender>` — 由 `GrpcSession.push_tx` 替代

7. **更新所有引用**：
   - `grpc/server.rs` — 删除 `ipc_session_mgr` 字段、删除双注册逻辑
   - `cron/mod.rs` — `session_mgr.find_by_agent_id()` 改为 `grpc_session_mgr.find_by_agent_id()`
   - `intent/router.rs` — 同上
   - `http/routes.rs` — `AppState.session_mgr` 字段删除
   - `gateway/mod.rs` — 不再创建 `crate::ipc::session::SessionManager::new()`

**风险点**：

| 风险 | 等级 | 处理 |
|------|------|------|
| cron handler 按 `conn_id` 查 session | 🟡 | cron handler 改为接收 `agent_id` 参数，不依赖连接层 |
| `intent/router.rs` async/sync route 依赖 `session_mgr` | 🟡 | 改为接收 `&SharedGrpcSessionMgr` |
| `GatewayGrpcService` 去除 Branch 2 后 push 路径仍通 | 🔴 高 | 需逐一审计所有 push 调用方 |
| dispatch.rs 中 `get_session(conn_id)` 查 `agent_id` | 🟡 | 由 GrpcSession 提供同等能力 |

**审计清单**：每个 push 到 Runtime 的 GatewayResponse 必须确认走的是 `GrpcSessionManager::push_to_agent()` 而不是旧的 `Session::push_message()`：

| push 调用源 | 当前路径 | 新路径 |
|-------------|----------|--------|
| `http/agents.rs:742` | `push_message(GatewayResponse::RuntimeConfigUpdate)` | ✅ 已走 `SharedGrpcSessionMgr` |
| `http/agents.rs:1010` | 同上 | ✅ |
| `http/agents.rs:2030` | 同上 | ✅ |
| `http/agents.rs:2457` | 同上 | ✅ |
| `http/embedding_api.rs` | `build_embed_sidecar_payload` → push | ✅ |
| `lifecycle/embed_supervisor.rs` | `Pusher::push_sidecar_endpoint()` | ✅ |
| `lifecycle/lsp_relay_supervisor.rs` | 同上 | ✅ |
| `lifecycle/manager.rs:190` | `push_message(GatewayResponse::EnableDebugMode)` | ✅ |
| `intent/router.rs:174` | `push_message(GatewayResponse::IntentReceived)` | 需确认 |
| `http/question.rs:75` | `push_message(GatewayResponse::IntentReceived)` | 需确认 |

### Commit C3：`global_push.rs` → `grpc/resource_pusher.rs`

**操作**：
1. `git mv core/acowork-gateway/src/ipc/global_push.rs` → `core/acowork-gateway/src/grpc/resource_pusher.rs`
2. 更新 `core/acowork-gateway/src/grpc/mod.rs`：
   ```rust
   pub mod resource_pusher;
   pub use resource_pusher::GlobalResourcePusher;
   ```
3. 更新 6 处 `use crate::ipc::global_push::*` → `use crate::grpc::resource_pusher::*`
4. 如果 C1 已删除 `pub mod ipc;`，此时可以**彻底删除 `gateway/src/ipc/` 目录**
5. 更新路径引用：`lifecycle/embed_supervisor.rs:49-51` 中的 `ipc::server` 提及 → `handlers::SharedState`

### Commit C4：Runtime 端删除 `pub mod ipc`

**操作**：
1. `rm -rf core/acowork-runtime/src/ipc/`
2. 编辑 `core/acowork-runtime/src/lib.rs:15`：删除 `pub mod ipc;`
3. `cargo build` 验证无 break

### Commit C5：配置/CLI 重命名

**操作**：

1. **`GatewayConfig::socket_path` 字段删除**（`gateway/config.rs:61, 350, 369-373, 525, 530, 597`）：
   - 删除字段定义
   - 删除 `default()` 中的 `default_socket` 计算逻辑
   - 删除 `from_cli()` 中的 `socket_path` 赋值
   - 删除测试断言 `assert!(!config.socket_path.is_empty());`

2. **Gateway CLI `--socket-path` / `ACOWORK_GATEWAY_SOCKET_PATH` 删除**（`gateway/cli.rs:54`）

3. **`RuntimeConfig::gateway_socket` → `RuntimeConfig::gateway_endpoint`**（`runtime/config.rs:52, 209, 236-239, 251-254`）

4. **Runtime CLI `--gateway-socket` / `ACOWORK_GATEWAY_SOCKET`** 改为 deprecated alias（`runtime/cli.rs:56-57`）：
   ```rust
   #[arg(long, env = "ACOWORK_GATEWAY_SOCKET", hide = true)]
   pub gateway_socket: Option<String>,   // deprecated, kept for back-compat
   ```

5. **`AgentBootContext::socket_path` → `AgentBootContext::endpoint`**（`runtime/startup/context.rs:82, agent_init.rs:504, 535`）

6. **`run_gateway_loop` 参数**：`_socket_path: String` 参数删除（`runtime/cli.rs:532, gateway_loop.rs:76`）

7. **Gateway 端 `spawn_agent_process` 参数名**（`lifecycle/process.rs:70`）：
   `--gateway-socket` → `--gateway-endpoint`
   保持 `--gateway-socket` 作为兼容 alias 继续工作（C6 注释清理后再删）

8. **测试 fixture**（`runtime/cli.rs:3399-3438`）：`unix:///tmp/gateway.sock` → `http://127.0.0.1:19877`

### Commit C6：proto 包名 + 注释清理

**实际执行**：

1. `core/acowork-core/proto/gateway_ipc.proto:3`：
   ```diff
   - package acowork.ipc.v1;
   + package acowork.gateway.v1;
   ```
   → prost 通过 `tonic::include_proto!("acowork.gateway.v1")` 正常生成代码，无需额外 `package_file_name` 配置。

2. `core/acowork-core/build.rs`：无变化。`build.rs` 引用的是 `.proto` 文件路径而非包名，`package` 变更不影响构建。

3. 注释清理（实测完成）：
   - `gateway/src/gateway/mod.rs:4`：`IPC server` → `gRPC server`
   - `gateway/src/gateway/mod.rs:376`：`IPC server` → `gRPC server`（含 `IPC connection handlers` → `gRPC connection handlers`）
   - `gateway/src/gateway/mod.rs:818`：`ipc_session_mgr` → `grpc_session_mgr`
   - `gateway/src/gateway/state.rs:70`：`same as IPC server` → `same as gRPC server`
   - `gateway/src/grpc/mod.rs:4-5`：`alternative to IPC transport` → `sole transport`
   - `gateway/src/grpc/dispatch.rs:4`：`used by the IPC server` → `used by the gRPC server`
   - `gateway/src/grpc/dispatch.rs:454`：`Mirrors the IPC server's...` → `Mirrors handle_session_response...`
   - `gateway/src/http/server.rs:3`：`alongside the IPC server` → `alongside the gRPC server`
   - `gateway/src/http/mod.rs:4`：`with the IPC server` → `with the gRPC server`
   - `gateway/src/http/chat.rs:196,399,502`：`IPC session` → `gRPC session`
   - `gateway/src/http/agents.rs:2066,2072,2290,2296`：`IPC session manager` → `gRPC session manager`（日志字符串）
   - `gateway/src/intent/router.rs:163,198,201`：`IPC session` → `gRPC session`（注释 + 错误消息）
   - `gateway/src/cron/mod.rs:366`：`IPC session` → `gRPC session`
   - `runtime/src/agent/loop_.rs:1315`：`ipc_client: None...` → 更新为描述 `chunk_tx` / `conversation`
   - `runtime/src/grpc/client.rs:9`：`legacy IPC client` → `legacy GatewayClient`
   - `gateway/src/handlers/server.rs:895`：保留历史说明 `no longer using legacy IPC transport`

4. **proto 文件名不改**：`gateway_ipc.proto` 保留原名（git diff 可追踪，build.rs 路径无需改）。

---

## 风险评估

| 风险 | 等级 | 缓解措施 |
|------|------|----------|
| **C2 中 Branch 2 删除后 push 路径断裂** | 🔴 高 | 审计所有 push 调用方（见上文审计清单），每个路径加 tracing! debug |
| **C2 handler 签名变更后忘记更新 dispatch 调用** | 🟡 中 | cargo build 是完整编译器检查，遗漏不会被放过 |
| **C6 proto 包名 break 外部 gRPC schema** | 🟡 中 | CHANGELOG 标注 breaking change；当前仅自家 Runtime 使用 |
| **C5 CLI 参数删除影响外部脚本** | 🟡 中 | `--gateway-socket` 保留为 deprecation alias |
| **cron 模块 session lookup 改成 agent_id 后语义漂移** | 🟡 中 | 加测试覆盖 cron trigger 场景 |
| **intent/router.rs `session_mgr.lock().await` 死锁** | 🟢 低 | 原路径已是双锁模式，改后减少锁数量，反而更安全 |

---

## 验证清单

**每个 commit 后**：
```bash
cd core
cargo build --release       # 零错误
cargo clippy --all-targets -- -D warnings  # 零警告
cargo test                  # 全测通过（已知失败除外）
```

**C6 完成后（冒烟测试）**：

- [x] `cargo build --release` 通过
- [x] `cargo clippy` 零警告
- [x] `cargo test` 全部通过
- [ ] 启 Gateway → 能正常启动，无 "failed to bind socket" 类错误
- [ ] Desktop App 连 Gateway → 启动 System Agent → 发消息收到回复
- [ ] 修改 Provider API Key → 热推送 → Runtime 日志显示 ProviderListUpdate
- [ ] 修改 MCP 配置 → 热推送 → Runtime 日志显示 SearchConfigDelivery
- [ ] 切模型 → 重启 Runtime → AgentHello 握手成功
- [ ] 手动 tool_approval_needed → 桌面弹窗 → 批准 → Runtime 收到
- [ ] DevMode debug panel（HTTP RPC + MQTT events，ADR-048）正常工作
- [ ] 检查 Gateway 日志：无 ERROR"missing session" / "unauthenticated session" 类异常

---

## 不（明确边界）

- **Desktop App 不做任何改动** — 桌面端的 gRPC client（TypeScript）是独立代码，本次只清理 Rust 端
- **`doc/design/zh/16-ipc-grpc-migration.md` 不修改** — 该设计文档是迁移时的历史记录，保留原样
- **`gateway_ipc.proto` 文件名不改** — 只有 proto 内部的 `package` 名改
- **`build.rs` 不改** — 如果 proto 包名变更不影响 prost 生成的 package 路径，则不动 build.rs
- **acowork-core 自身的 `pub use` 链不做重导出改名** — 保持 `acowork_core::protocol::GatewayRequest` 等公开 API 不变

---

## 后续清理（ADR-031 范围之外，留待后续 ADR 决策）

以下清理点属于"明确边界"外的延伸，**不影响传输层正确性**，但可以考虑未来另立 ADR 决议：

### 1. 公开 API 错误变体名 `Ipc(String)`

3 个 crate 的 `pub enum Error { ..., Ipc(String), ... }`：
- `acowork_core::AcoworkError::Ipc`（在 `acowork-core/src/error.rs:55`）
- `acowork_gateway::AcoworkError::Ipc`（在 `acowork-gateway/src/error.rs:19`）
- `acowork_runtime::RuntimeError::Ipc`（在 `acowork-runtime/src/error.rs:21`）

**决策**：保留 `Ipc` 变体名（公开 API 不变原则）。调用方用 `match` 模式匹配，重命名 = breaking change。Display message `"IPC error: {0}"` 同步保留。

**未来如果重命名**：`Ipc` → `GatewayTransport` / `GrpcTransport`，需要配合 major version bump + changelog 文档。

### 2. 公开常量 `SESSION_IPC` 命名

- `acowork_core::timeout_config::constants::SESSION_IPC`（在 `acowork-core/src/timeout_config.rs:235`）
- `acowork_gateway::http::chat::SESSION_IPC_TIMEOUT`（在 `acowork-gateway/src/http/chat.rs:1748`）

**决策**：保留原名（同公开 API 原则）。内部注释已更新为"gRPC"。

### 3. 内部测试 fixture 与 deprecated alias

- `runtime/src/cli.rs:3406` 的测试 `test_cli_gateway_socket_arg` 仍使用 `unix:///tmp/gateway.sock`，保留作为 deprecated alias 的回归测试。
- `apps/acowork-desktop/src/lib/types.ts:166` 的 `GatewayConfig.socket_path` 字段定义：根据本 ADR §"不（明确边界）"决定不改 Desktop App 侧。运行时 `ConfigResponse` 已不含该字段，TypeScript 类型与运行时存在不一致但不影响功能（字段 undefined）。

### 4. C5b 补漏（非原子提交追加）

`DataFlowConfig::ipc_push_capacity` 字段（C5 漏删除）：
- 定义在 `acowork-gateway/src/config.rs:138-141, 157, 170`
- 整 workspace 0 消费方
- 与同结构 `grpc_outbound_capacity` 字段语义重复
- 在 ADR-031 收尾批（commit "C5b 补漏"）已删除

### 5. ADR-031 实施偏差记录

以下偏差是合理有意识的（已在 commit message 和本 ADR §"实施计划 / Commit C6 / 实际执行"段注明）：

| 项目 | 偏差 | 原因 |
|------|------|------|
| `handlers/server.rs` 而非 `handlers.rs` | ADR 写"git mv → handlers.rs"，实际用目录 `handlers/server.rs` | 后续需要 session_state 等子模块，目录化更可扩展 |
| `handlers/session_state.rs` 不存在 | ADR 列出该目标文件，实际未创建 | C2 把 `Session`/`SessionManager` 整体合并到 `GrpcSession`，无内容保留 |
| C6 测试 fixture 改写 | ADR 第 8 条建议改写，实际保留 deprecated test fixture | 用于验证 deprecated alias `--gateway-socket` 解析 |
| `gateway_ipc.proto` 文件名 | ADR §"不"决定保留 | git diff 可追踪，build script 路径不变 |

---

## 附录：完整文件变更清单

| 文件 | C1 | C2 | C3 | C4 | C5 | C6 |
|------|:--:|:--:|:--:|:--:|:--:|:--:|
| `gateway/src/ipc/server.rs` → `gateway/src/handlers.rs` | ✅ | ✅ | | | | |
| `gateway/src/ipc/session.rs` → `gateway/src/handlers/session_state.rs` | ✅ | ✅ | | | | |
| `gateway/src/ipc/mod.rs` | 改 | ✅ | ❌删 | | | |
| `gateway/src/ipc/global_push.rs` → `gateway/src/grpc/resource_pusher.rs` | | | ✅ | | | |
| `gateway/src/grpc/mod.rs` | ✅ | | ✅ | | | |
| `gateway/src/grpc/dispatch.rs` | ✅ | ✅ | | | | |
| `gateway/src/grpc/server.rs` | ✅ | ✅ | | | | |
| `gateway/src/handlers/mod.rs` | **新** | | | | | |
| `gateway/src/lib.rs` | ✅ | | | | | |
| `core/proto/gateway_ipc.proto` | | | | | | ✅ |
| `acowork-core/build.rs` | | | | | | 无需改 |
| `acowork-core/src/lib.rs` | | | | | | ✅ |
| `gateway/src/gateway/mod.rs` | ✅ | ✅ | ✅ | | | ✅(注释) |
| `gateway/src/gateway/state.rs` | | | | | | ✅(注释) |
| `gateway/src/grpc/mod.rs` | ✅ | | ✅ | | | ✅(doc) |
| `gateway/src/grpc/dispatch.rs` | ✅ | ✅ | | | | ✅(doc) |
| `gateway/src/http/server.rs` | | | ✅ | | | ✅(doc) |
| `gateway/src/http/mod.rs` | | | | | | ✅(doc) |
| `gateway/src/http/chat.rs` | | | | | | ✅(注释) |
| `gateway/src/http/agents.rs` | | | | | | ✅(日志) |
| `gateway/src/intent/router.rs` | ✅ | ✅ | | | | ✅(注释) |
| `gateway/src/cron/mod.rs` | ✅ | ✅ | | | | ✅(doc) |
| `gateway/src/handlers/server.rs` | ✅ | ✅ | | | | ✅(注释) |
| `runtime/src/agent/loop_.rs` | | | | | | ✅(注释) |
| `runtime/src/grpc/client.rs` | | | | | | ✅(doc) |
| 合计文件数 | ~16 | ~8 | ~8 | ~3 | ~8 | ~4 |
