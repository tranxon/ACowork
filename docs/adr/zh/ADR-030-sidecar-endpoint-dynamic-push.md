# ADR-030：Sidecar 端点动态推送 — Gateway → Runtime

**状态**：已完成（C1 ✅ C2 ✅ C3 ✅ C4 ✅）
**日期**：2026-07-08
**决策者**：大鱼
**前置**：
- ADR-019（LSP Relay 解耦为独立进程）
- ADR-029（Builtin Tools 持久化与使能控制 — agent_tools.json）

---

## 决策摘要

**4 个 commit，每个独立 buildable（用户选定"走法 B"）**：

| Commit | 范围 | 状态 |
|--------|------|------|
| **C1** | `SidecarEndpointUpdate` 消息 + `SidecarKind` enum + proto + bridge + grpc client decode（**纯新增**） | ✅ HEAD 已完成 |
| **C2** | Gateway `GlobalResourcePusher::push_sidecar_endpoint()` + **embed supervisor 迁移**（保留 `push_embedding_config()` 为 deprecated wrapper） | ⏳ 待做 |
| **C3** | Runtime `register_dynamic_tool()` / `unregister_dynamic_tool()` + cli.rs 路由 `SidecarEndpointUpdate` + **LSP relay supervisor 接 pusher** + agent_tools.json 默认启用 codebase | ⏳ 待做 |
| **C4** | **清理**：从 `RuntimeConfigUpdate` 移除 `embed_config_json` 字段 + 从 `GatewayResponse` 移除 `EmbeddingConfigUpdate` variant + `push_embedding_config()` 函数移除 | ⏳ 待做 |

**关键决策**（按对话时序）：

| 决策 | 来源 | 内容 |
|------|------|------|
| 不做 L1 Readiness Barrier | 用户 2026-07-08 05:23:36 | "前端/agent都死等这些进程ready是不合理的。目前的agent hello是合理的，子进程的ready应该单独处理"——AgentHello 维持现状，pusher 异步补 |
| 新增独立消息而不是内嵌字段 | 用户 2026-07-08 06:20:38 | `SidecarEndpointUpdate` 是 `GatewayResponse` 的新 variant，不塞 `RuntimeConfigUpdate` |
| embed 完全迁过来 | 用户 2026-07-08 06:22:13 | "直接升级，项目还在开发中，没有兼容性需求"——embed 也走 `SidecarEndpointUpdate`，老通道最终删除 |
| 前端先不改 | 用户 2026-07-08 06:20:38 | 本次 4 commit 不动 Desktop App（右侧工具面板等 C4 之后再单独立项） |
| 走法 B（4 commit） | 用户 2026-07-08 06:33:49 | "走B吧，一步一个脚印" |

---

## 影响范围

### C1（已完成）

**新增**：
- `core/acowork-core/proto/gateway_ipc.proto`：`SidecarKind` enum + `SidecarEndpointUpdate` message + `ServerMessage.payload` 新 tag 44
- `core/acowork-core/src/protocol.rs`：`SidecarKind` enum + `GatewayResponse::SidecarEndpointUpdate` variant
- `core/acowork-core/src/proto_bridge.rs`：`sidecar_to_proto()` + `SidecarEndpointUpdate` 双向转换
- `core/acowork-runtime/src/grpc/client.rs`：`proto_to_gateway_response()` 解码 `SidecarEndpointUpdate` → `GatewayResponse::SidecarEndpointUpdate`

**保留**（C4 才删）：
- `RuntimeConfigUpdate.embed_config_json` 字段
- `GatewayResponse::EmbeddingConfigUpdate` variant
- `GlobalResourcePusher::push_embedding_config()` 函数

**额外（C1 顺手加的）**：
- `GatewayRequest::UpdateConfig` 增加 `builtin_tools_enabled_json` / `builtin_tools_all_json` 字段（用于 ADR-029 双向同步）
- `GatewayResponse::RuntimeConfigUpdate` 增加 `builtin_tools_enabled: Option<Vec<String>>` 字段

### C2（待做）

**修改**：
- `core/acowork-gateway/src/ipc/global_push.rs`：新增 `push_sidecar_endpoint(sidecar: SidecarKind, endpoint: String, spec_json: String)` 通用方法；`push_embedding_config()` 标记 `#[deprecated(note = "use push_sidecar_endpoint(SidecarKind::Embed, ...) instead")]`，函数体改为调用 `push_sidecar_endpoint(SidecarKind::Embed, ...)`（保留为薄壳）
- `core/acowork-gateway/src/lifecycle/embed_supervisor.rs`：4 处 `pusher.push_embedding_config().await` 调用迁移到 `pusher.push_sidecar_endpoint(SidecarKind::Embed, endpoint, spec_json).await`（endpoint / spec_json 从 `gw.embed_process` 派生）
- `core/acowork-gateway/src/http/embedding_api.rs`：1 处 `pusher.push_embedding_config().await` 调用迁移

**不动**（C3 才动）：
- `core/acowork-gateway/src/lifecycle/lsp_relay_supervisor.rs`——C2 期间 LSP relay supervisor 不接 pusher

### C3（待做）

**修改**：
- `core/acowork-runtime/src/tools/registry.rs`：`ToolRegistry` 加 `register_external(spec, factory)` / `unregister(name)` API；内部从 `Vec<Arc<dyn Tool>>` 改为 `Arc<RwLock<Vec<...>>>`，保证线程安全；`all()` / `tool_names()` / `activate()` 走 RwLock
- `core/acowork-runtime/src/agent/agent_core.rs` 或 `session/session_manager.rs`：暴露 `register_dynamic_tool(name, Arc<dyn Tool>)` / `unregister_dynamic_tool(name)` 业务方法，封装"加/删 entry + rebuild all_tools + broadcast 到所有 session"
- `core/acowork-runtime/src/cli.rs`：`GatewayResponse::SidecarEndpointUpdate` 处理分支，根据 `sidecar` 路由：
  - `LspRelay` + endpoint 非空 → `register_dynamic_tool("codebase", CodebaseTool::new(endpoint))`
  - `LspRelay` + endpoint 空 → `unregister_dynamic_tool("codebase")`
  - `Embed` + endpoint 非空 → 解析 spec_json，调用 `embedding_manager.rebuild_provider_chain(endpoint, model_id, dimension)`
  - `Embed` + endpoint 空 → 清空 ONNX provider，回退到纯远端 fallback
- `core/acowork-gateway/src/lifecycle/lsp_relay_supervisor.rs`：
  - `LspRelaySupervisorConfig` 增加 `pusher: Option<Arc<GlobalResourcePusher>>` 字段
  - `start_lsp_relay_supervisor(cfg, state)` 签名加 pusher
  - 关键状态转换点调用 `pusher.push_sidecar_endpoint(SidecarKind::LspRelay, endpoint, "".to_string())`：
    - SSE 连上、mark ready 后（lsp_relay_supervisor.rs:271-278）
    - 重启成功后（line 162-165）
    - reaper 检测到子进程退出（line 172-176）
    - 重启超限放弃（line 145-147）
    - 重启前清空（line 137-139）
- `core/acowork-gateway/src/lifecycle/mod.rs` + `gateway/mod.rs`：`start_lsp_relay_supervisor` 调用处传入 pusher
- `core/acowork-runtime/src/tools/builtin/mod.rs`：`all_builtin_tools()` **保留** `lsp_relay_endpoint: Option<String>` 启动期注册逻辑（与 AgentHello 快照协作）；动态注册由 `register_dynamic_tool` 负责后续变更（同名替换，不重复）
- `core/acowork-runtime/src/startup/agent_init.rs`：保持现状——启动时根据 `hello_config.lsp_relay_endpoint` 决定是否注册 codebase

**新增 / 修改**：
- 各 agent 包 `{work_dir}/config/agent_tools.json`：senior-engineer 包默认 `codebase.enabled = true`（依赖 sidecar push 注册）

**不动**：
- Desktop App 前端（用户 06:20:38："前端先不改"）

### C4（待做）

**清理**：
- `core/acowork-core/src/protocol.rs`：
  - 从 `RuntimeConfigUpdate` 移除 `embed_config_json: Option<String>` 字段
  - 从 `GatewayResponse` 移除 `EmbeddingConfigUpdate` variant
  - 从 `GatewayResponse::RuntimeConfigUpdate` 移除 `embed_config_json` 字段（如果存在）
- `core/acowork-core/src/proto_bridge.rs`：删除 `embed_config_json` 双向转换代码
- `core/acowork-runtime/src/grpc/client.rs`：删除 `RuntimeConfigUpdate.embed_config_json` 接收分支
- `core/acowork-runtime/src/cli.rs`：删除 `GatewayResponse::EmbeddingConfigUpdate` 接收分支
- `core/acowork-gateway/src/ipc/global_push.rs`：
  - 删除 `push_embedding_config()` 函数
  - 删除 `embed_config_json` 字段在 `RuntimeConfigUpdate` 构造处的填充
- `core/acowork-gateway/src/http/embedding_api.rs`：删除 `push_embedding_config()` 调用（已迁移到 `push_sidecar_endpoint`，调用点可同步精简）
- `core/acowork-gateway/src/lifecycle/embed_supervisor.rs`：删除 4 处 `push_embedding_config()` 调用（C2 已迁移到 `push_sidecar_endpoint`，可同步精简注释）

**新增约束**：
- 老 runtime（未升级到 C1+）首次 AgentHello 仍能拿到 `embed_endpoint`（`AgentHelloResult` 字段保留）；但**运行中** embed 模型切换/重启不再有推送通道
- 这是用户接受的取舍（06:22:13："直接升级，项目还在开发中，没有兼容性需求"）

---

## 背景

### 现状

Gateway 当前管理两个 sidecar 进程：
- **embed** (`acowork-embed`)：ONNX 本地嵌入推理 HTTP 服务，端口 18080
- **lsp_relay** (`acowork-lsp-relay`)：LSP 协议 JSON-RPC 中继，端口 19878

Runtime 启动时通过 `GatewayResponse::AgentHelloResult` 拿到初始端点（line 812-833 of `protocol.rs`）：
- `embed_endpoint` / `embed_model_id` / `embed_dimension` — 用于构建 `FallbackEmbeddingProvider` 链
- `lsp_relay_endpoint` — 用于 `all_builtin_tools()` 决定是否注册 `codebase` tool

启动后这两个 sidecar 的状态变化通过两条**不同通道**推送：
- embed：`GatewayResponse::RuntimeConfigUpdate.embed_config_json` 字段（`global_push.rs:348-437` 的 `push_embedding_config()`），格式是 JSON 套 JSON
- lsp_relay：**完全没有推送通道**

### 问题 1：LSP relay 状态变化无推送通道

`lsp_relay_supervisor.rs` 的状态机比 embed 更复杂：
- 启动后 SSE 连接成功时设置 `lsp_relay_process.ready = true`（`lsp_relay_supervisor.rs:271-278`）
- Heartbeat 超时 / 连接丢失 / 重启 / 重启失败 attach 现有进程
- Reaper 任务检测到子进程退出时清空 `lsp_relay_process`（line 172-176）

这些状态变化 Runtime 完全感知不到。如果 Runtime 启动时 LSP relay 还没 ready（典型场景：Gateway 刚启动，supervisor 还在 30s 启动宽限期内），codebase tool 就**不会注册**，后续 LSP relay ready 了也不会自动注册。

### 问题 2：codebase tool 启动期一次性注册，无动态增删

当前 `all_builtin_tools()` 在启动时根据 `lsp_relay_endpoint: Option<String>` 一次性决定是否注册 codebase tool：

```rust
// core/acowork-runtime/src/tools/builtin/mod.rs:140-145
// Only register codebase when the LSP Relay is available.
// Without the relay, the tool always fails with "LSP Relay not available",
// wasting LLM inference tokens on doomed calls.
if let Some(endpoint) = lsp_relay_endpoint {
    tools.push(Arc::new(codebase::CodebaseTool::new(endpoint)));
}
```

`ToolRegistry.tools: Vec<Arc<dyn Tool>>` 是不可变集合，没有 `add_tool()` / `remove_tool()` 方法。Registry 在 `startup/agent_init.rs:353-365` 注册后就不变了。

### 问题 3：`embed_config_json` 是 JSON 套 JSON

```rust
// 当前推送格式
embed_config_json: Some(serde_json::json!({
    "embed_endpoint": "http://127.0.0.1:18080/v1",
    "embed_model_id": "bge-small-zh-v1.5",
    "embed_dimension": 512,
}).to_string()),
```

字段名带前缀（`embed_endpoint` / `embed_model_id` / `embed_dimension`）是因为它要在一个 JSON 里承载多个相关字段。Runtime 端再 `serde_json::from_str` 一层。这套机制**不可扩展**——再加一个 sidecar 又要在 `RuntimeConfigUpdate` 加一个 JSON 字段，proto 加 N 个独立字段。

### 问题 4：agent_tools.json 缺 codebase

`{work_dir}/config/agent_tools.json` 当前 16 个工具，缺 `codebase`。C3 完成后 codebase 工具会通过 sidecar push 动态注册到 registry，但 agent_tools.json 里没列就会**默认 disabled**（registry 里有但被 `enabled_entries` 过滤掉）。

### 已否决的方案：L1 Readiness Barrier

最初提议在 `gateway/mod.rs` 启动时阻塞等待所有 sidecar ready 后再 accept gRPC。**用户否决**（05:23:36）：

> "你忽略了几个事实：1. embed/lsp是进程启动初始化，延迟是秒级，后续可能还会有其他子进程引入，等所有进程ready，延迟不可接受 2.对于前端和agent runtime来说，他不必等所有gateway进程资源启动之后再启动... 所以前端/agent都死等这些进程ready是不合理的。目前的agent hello是合理的，子进程的ready应该单独处理。"

**真正的缺口**：runtime 拿到 None 后，后续 sidecar 真的 ready 了，runtime **收不到通知也没法补救**——这才是本 ADR 要解决的问题。

---

## 目标

1. **通用化 sidecar 推送通道**：embed 和 lsp_relay 共用一条消息类型 `SidecarEndpointUpdate`
2. **解耦推送语义**：不再用 `RuntimeConfigUpdate` 内嵌 JSON 字段
3. **支持动态注册/卸载 builtin tools**：ToolRegistry 增加 `add_tool()` / `remove_tool()` 线程安全接口
4. **侧车生命周期全感知**：Runtime 能感知 sidecar 从无 → ready → 端点变化 → 不可用 的全部状态
5. **冷启动零延迟**：AgentHello 维持现状（不阻塞等 sidecar ready），后续 sidecar ready 由 push 异步补
6. **过渡期兼容**：C2/C3 期间 `embed_config_json` 字段保留，C4 一次性清理（用户已确认不需要兼容老 runtime）

---

## 详细设计

### Phase C1：协议层（✅ 已完成）

> **目标**：在 wire-protocol 上建立通用 sidecar 推送通道，**不删任何旧字段**
> **当前状态**：协议字段、proto、client.rs 解码全部就位（HEAD）

#### C1.1 新增 `SidecarKind` enum

```rust
// core/acowork-core/src/protocol.rs
/// Identifies a Gateway-managed sidecar process. The Runtime uses this to
/// route a `SidecarEndpointUpdate` to the correct subsystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidecarKind {
    /// Reserved for forward-compat. Treated as "unknown" by the Runtime.
    Unspecified,
    /// acowork-lsp-relay — provides JSON-RPC LSP relay used by the
    /// Runtime's `codebase` builtin tool.
    LspRelay,
    /// acowork-embed — local ONNX embedding HTTP service. The Runtime
    /// builds a `FallbackEmbeddingProvider` chain from the active model
    /// id and dimension provided in the push payload.
    Embed,
}

impl SidecarKind {
    pub fn as_str(&self) -> &'static str { ... }
}
impl std::str::FromStr for SidecarKind { ... }
```

- `as_str()` / `FromStr` 提供稳定的 wire 字符串表示
- 新增 sidecar 只需追加 enum 变体 + proto 加值（never rename / reorder）

#### C1.2 新增 `GatewayResponse::SidecarEndpointUpdate` 消息

```rust
/// Sidecar endpoint update (Gateway → Runtime, push).
SidecarEndpointUpdate {
    /// Which sidecar this update is for.
    sidecar: SidecarKind,
    /// HTTP URL the Runtime should use. Empty string = sidecar unavailable.
    endpoint: String,
    /// Sidecar-specific metadata. Schema depends on `sidecar`:
    ///   - LspRelay: "" (no extra fields today)
    ///   - Embed:    {"model_id":"bge-small-zh-v1.5","dimension":512}
    /// Empty string if no metadata applies.
    spec_json: String,
},
```

**关键决策**：**空字符串 = sidecar 不可用**（而不是用 `Option<String>` + None 字段）。proto 字段不能表达 `Option<String>` 的"有/无"二义性，空字符串是天然的"无"标记。

#### C1.3 proto 绑定

```protobuf
// core/acowork-core/proto/gateway_ipc.proto
enum SidecarKind {
    SIDECAR_KIND_UNSPECIFIED = 0;
    SIDECAR_KIND_LSP_RELAY = 1;
    SIDECAR_KIND_EMBED = 2;
}

message SidecarEndpointUpdate {
    SidecarKind sidecar = 1;
    string endpoint = 2;       // empty = unavailable
    string spec_json = 3;      // empty = no extra fields
}

// ServerMessage.payload 加 tag 44
```

`proto_bridge.rs` 提供 `sidecar_to_proto()` / `sidecar_from_proto()` 转换函数 + `SidecarEndpointUpdate` 双向转换。

#### C1.4 gRPC client 解码

```rust
// core/acowork-runtime/src/grpc/client.rs:1310-1331
Some(ServerPayload::SidecarEndpointUpdate(seu)) => {
    let sidecar = match seu.sidecar {
        x if x == proto::SidecarKind::LspRelay as i32 => SidecarKind::LspRelay,
        x if x == proto::SidecarKind::Embed as i32 => SidecarKind::Embed,
        _ => SidecarKind::Unspecified,
    };
    tracing::info!(sidecar = %sidecar.as_str(), endpoint = %seu.endpoint, ...);
    GatewayResponse::SidecarEndpointUpdate { sidecar, endpoint: seu.endpoint, spec_json: seu.spec_json }
}
```

**C1 边界**：client 解码完成后只记录日志，**不**真正路由到 AgentCore——这是 C3 的工作。

#### C1 验收

- ✅ `cargo build --workspace` 通过
- ✅ `cargo test --workspace` 通过
- ✅ `SidecarKind` 单元测试覆盖 wire 字符串稳定性
- ✅ proto ↔ domain 双向转换单测
- ✅ 老 runtime 仍能工作（`embed_config_json` 字段未删）

---

### Phase C2：Gateway 推送层（待做）

> **目标**：把 `push_sidecar_endpoint()` 实现为通用推送通道，把 embed supervisor 迁移过去
> **LSP relay supervisor 不动**（C3 才接 pusher）
> **不动 runtime**

#### C2.1 `GlobalResourcePusher::push_sidecar_endpoint()` 新方法

```rust
// core/acowork-gateway/src/ipc/global_push.rs
/// Push a sidecar endpoint update to all running agents.
/// This is the canonical channel for sidecar state changes
/// (lsp_relay ready, embed model switched, sidecar crash, ...).
/// Empty `endpoint` signals "sidecar is unavailable" — the Runtime
/// should disable dependent features rather than try to connect.
#[tracing::instrument(skip(self), name = "push_sidecar_endpoint")]
pub async fn push_sidecar_endpoint(
    &self,
    sidecar: SidecarKind,
    endpoint: String,
    spec_json: String,
) {
    let grpc_session_mgr = match &self.grpc_session_mgr {
        Some(mgr) => mgr.clone(),
        None => {
            tracing::warn!(sidecar = %sidecar.as_str(), "No gRPC session manager, skipping sidecar push");
            return;
        }
    };

    let agent_ids: Vec<String> = {
        let gw = self.gateway_state.read().await;
        gw.running_agents.keys().cloned().collect()
    };

    if agent_ids.is_empty() {
        return;
    }

    let mut pushed = 0u32;
    let mut failed = 0u32;
    for agent_id in agent_ids {
        let mgr = grpc_session_mgr.lock().await;
        if let Some((_conn_id, session)) = mgr.find_by_agent_id(&agent_id) {
            let ok = session.push_message(GatewayResponse::SidecarEndpointUpdate {
                sidecar,
                endpoint: endpoint.clone(),
                spec_json: spec_json.clone(),
            }).await;

            if ok {
                tracing::info!(agent = %agent_id, sidecar = %sidecar.as_str(), "Pushed sidecar endpoint to agent");
                pushed += 1;
            } else {
                tracing::warn!(agent = %agent_id, sidecar = %sidecar.as_str(), "Sidecar push failed (channel closed)");
                failed += 1;
            }
        }
    }

    if pushed > 0 || failed > 0 {
        tracing::info!(sidecar = %sidecar.as_str(), pushed, failed, "Sidecar push complete");
    }
}
```

#### C2.2 标记 `push_embedding_config()` 为 deprecated

```rust
/// DEPRECATED: Use `push_sidecar_endpoint(SidecarKind::Embed, ...)` instead.
/// This method now delegates to the generic sidecar channel but is
/// retained for backward compatibility with external callers (tests,
/// ad-hoc scripts). It will be removed in C4.
#[deprecated(note = "use push_sidecar_endpoint(SidecarKind::Embed, ...) instead")]
#[tracing::instrument(skip(self), name = "push_embedding_config")]
pub async fn push_embedding_config(&self) {
    // ... 原有 embed_endpoint 提取逻辑 ...
    self.push_sidecar_endpoint(SidecarKind::Embed, endpoint, spec_json).await;
}
```

这样外部测试代码继续可用，编译时会有 deprecation warning 提示迁移。

#### C2.3 迁移 embed supervisor 调用点

`lifecycle/embed_supervisor.rs` 现有 4 处 `pusher.push_embedding_config().await`（line 321、337、600、709）：

```rust
// 迁移前
if let Some(p) = &pusher {
    p.push_embedding_config().await;
}

// 迁移后
if let Some(p) = &pusher {
    p.push_sidecar_endpoint(SidecarKind::Embed, endpoint, spec_json).await;
}
```

`push_embedding_config()` 函数内部已经有从 `gw.embed_process` 提取 (endpoint, model_id, dimension) 的逻辑（global_push.rs:367-382），需要把这部分提取成一个**独立 helper 函数** `build_embed_sidecar_payload()`，让 `push_embedding_config()` 和 embed supervisor 都能调用，避免重复：

```rust
// global_push.rs
fn build_embed_sidecar_payload(state: &GatewayState) -> Option<(String, String)> {
    let eps = state.embed_process.as_ref()?;
    if eps.active_model_id.is_none() {
        return None;
    }
    let endpoint = format!("http://127.0.0.1:{}/v1", eps.port);
    let spec_json = serde_json::json!({
        "model_id": eps.active_model_id.clone().unwrap_or_default(),
        "dimension": eps.active_dimension.unwrap_or(0),
    }).to_string();
    Some((endpoint, spec_json))
}
```

embed supervisor 改为：

```rust
if let Some(p) = &pusher {
    let state_guard = state.read().await;
    if let Some((endpoint, spec_json)) = build_embed_sidecar_payload(&state_guard) {
        drop(state_guard);
        p.push_sidecar_endpoint(SidecarKind::Embed, endpoint, spec_json).await;
    }
}
```

#### C2.4 迁移 `embedding_api.rs` 调用点

`http/embedding_api.rs:443` 的 1 处 `pusher.push_embedding_config().await` 同步迁移到 `push_sidecar_endpoint(Embed, ...)`。

#### C2.5 LSP relay supervisor **不动**

`lsp_relay_supervisor.rs` 在 C2 期间**保持原状**——不接 pusher，状态变化不推送。codebase tool 仍然只在启动期根据 `AgentHelloConfig.lsp_relay_endpoint` 注册一次（如果该时刻 LSP relay 已 ready）。

C3 才会让 LSP relay supervisor 接入 pusher。

#### C2 验收

- `cargo build --workspace` 通过
- `cargo test --workspace` 通过（deprecation warning 可见但不破坏编译）
- `global_push.rs` 单测覆盖 `push_sidecar_endpoint()` 调用
- `embed_supervisor` 现有单测不破
- `lsp_relay_supervisor` 现有单测不破（C2 不动它）
- 端到端：embed 模型切换 → `push_sidecar_endpoint(Embed, ...)` 推送 → 老 runtime（仍读 `embed_config_json`）**也**能收到 embed config（通过 `push_embedding_config()` deprecated wrapper 兼容）
- 端到端：LSP relay ready / restart → 当前不推送（行为不变，C3 才接）

---

### Phase C3：Runtime 业务层 + LSP relay supervisor 接入（待做）

> **目标**：让 Runtime 真正响应 `SidecarEndpointUpdate` 消息，动态注册/卸载 `codebase` 工具，触发 embed provider 链重建
> **同步完成**：LSP relay supervisor 接 pusher（之前一直缺的）
> **不动前端**

#### C3.1 `ToolRegistry` 增加动态增删 API

当前 `tools/registry.rs:18-21`：

```rust
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}
```

改造为：

```rust
pub struct ToolRegistry {
    /// Internal mutable collection, protected by RwLock for thread safety.
    /// The legacy `Vec<Arc<dyn Tool>>` is replaced with `Arc<RwLock<Vec<...>>>`
    /// so `add_tool` / `remove_tool` can be called concurrently with
    /// `all_tools` / `tool_names` / `activate`.
    tools: Arc<RwLock<Vec<Arc<dyn Tool>>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: Arc::new(RwLock::new(Vec::new())) }
    }

    /// Register a tool. If a tool with the same name exists, it is replaced.
    /// No-op if the same instance is already registered.
    pub async fn register_external(&self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        let mut tools = self.tools.write().await;
        if let Some(existing) = tools.iter().position(|t| t.name() == name) {
            tools[existing] = tool;
            tracing::info!(tool = %name, "Replaced existing tool in registry");
        } else {
            tools.push(tool);
            tracing::info!(tool = %name, "Added tool to registry");
        }
    }

    /// Remove a tool by name. Returns true if found and removed.
    pub async fn unregister(&self, name: &str) -> bool {
        let mut tools = self.tools.write().await;
        let before = tools.len();
        tools.retain(|t| t.name() != name);
        let removed = tools.len() < before;
        if removed {
            tracing::info!(tool = %name, "Removed tool from registry");
        }
        removed
    }

    /// Async snapshot of the current tool list.
    pub async fn all_tools_snapshot(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.read().await.clone()
    }

    /// Synchronous accessor — returns a snapshot via try_read.
    /// None if lock is held (callers should fall back to async).
    pub fn all(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.try_read()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    // ... existing register() / tool_names() / activate() 改造 ...
}
```

**API 兼容性约束**：
- 现有 `register()` / `all()` / `tool_names()` / `activate()` 签名尽量保留，内部用 try_read / try_write 兜底
- 新增 `register_external()` / `unregister()` / `all_tools_snapshot()` 异步 API

#### C3.2 `AgentCore` 增加 `register_dynamic_tool` / `unregister_dynamic_tool`

在 `agent/agent_core.rs` 或 `session/session_manager.rs` 暴露业务方法：

```rust
impl AgentCore {
    /// Register a tool dynamically. Used by SidecarEndpointUpdate handler
    /// when lsp_relay becomes available. Also called by MCP hot-add flows.
    pub async fn register_dynamic_tool(&self, name: &str, tool: Arc<dyn Tool>) -> Result<()> {
        // 1. 注册到 ToolRegistry
        self.tool_registry.register_external(tool).await;
        // 2. rebuild all_tools（合并 builtin + dynamic）
        self.rebuild_all_tools().await?;
        // 3. broadcast 到所有活跃 session（让正在运行的 LLM loop 看到新 tool）
        self.broadcast_tool_change().await;
        Ok(())
    }

    /// Unregister a dynamic tool. Used when lsp_relay becomes unavailable.
    pub async fn unregister_dynamic_tool(&self, name: &str) -> Result<()> {
        if !self.tool_registry.unregister(name).await {
            return Ok(());  // 工具本来就没注册，幂等
        }
        self.rebuild_all_tools().await?;
        self.broadcast_tool_change().await;
        Ok(())
    }
}
```

#### C3.3 `cli.rs` 路由 `SidecarEndpointUpdate`

`cli.rs` 主循环加 `SidecarEndpointUpdate` 分支（放在 `RuntimeConfigUpdate` 分支旁边）：

```rust
GatewayResponse::SidecarEndpointUpdate { sidecar, endpoint, spec_json } => {
    match sidecar {
        SidecarKind::LspRelay => {
            if endpoint.is_empty() {
                agent_core.unregister_dynamic_tool("codebase").await?;
                tracing::info!("LSP Relay unavailable: removed codebase tool");
            } else {
                let tool: Arc<dyn Tool> = Arc::new(
                    crate::tools::builtin::codebase::CodebaseTool::new(endpoint.clone())
                );
                agent_core.register_dynamic_tool("codebase", tool).await?;
                tracing::info!(endpoint = %endpoint, "LSP Relay available: registered codebase tool");
            }
        }
        SidecarKind::Embed => {
            if endpoint.is_empty() {
                // embed 不可用：移除 ONNX provider
                embedding_manager.disable_onnx_provider().await?;
            } else {
                let spec: EmbedSidecarSpec = serde_json::from_str(&spec_json)
                    .map_err(|e| format!("invalid embed spec_json: {e}"))?;
                embedding_manager.enable_onnx_provider(
                    endpoint.clone(),
                    spec.model_id,
                    spec.dimension,
                ).await?;
            }
        }
        SidecarKind::Unspecified => {
            tracing::warn!("Received SidecarEndpointUpdate with Unspecified kind; ignoring");
        }
    }
    LoopAction::Continue
}
```

`EmbedSidecarSpec` 结构体：

```rust
#[derive(Debug, Deserialize)]
struct EmbedSidecarSpec {
    model_id: String,
    dimension: usize,
}
```

#### C3.4 LSP relay supervisor 接入 pusher

`LspRelaySupervisorConfig` 加 `pusher: Option<Arc<GlobalResourcePusher>>` 字段，`start_lsp_relay_supervisor()` 签名加 pusher 参数，run_supervisor 内部在 5 个状态变化点调用 `push_sidecar_endpoint(LspRelay, ...)`：

| 位置 | 事件 | 推送内容 |
|------|------|---------|
| `lsp_relay_supervisor.rs:271-278` | SSE 连接成功，mark ready | endpoint = `http://127.0.0.1:{port}`，spec = "" |
| `lsp_relay_supervisor.rs:137-139` | 重启前清空 lsp_relay_process | endpoint = ""，spec = "" |
| `lsp_relay_supervisor.rs:145-147` | 重启超限放弃 | endpoint = ""，spec = "" |
| `lsp_relay_supervisor.rs:162-165` | 重启成功（新 PID） | endpoint = `http://127.0.0.1:{port}`，spec = "" |
| `lsp_relay_supervisor.rs:172-176` | Reaper 检测到子进程退出 | endpoint = ""，spec = "" |

类似 embed，提取 helper：

```rust
fn build_lsp_relay_sidecar_payload(state: &GatewayState, default_port: u16) -> (String, String) {
    let endpoint = state.lsp_relay_process.as_ref()
        .filter(|p| p.ready)
        .map(|p| format!("http://127.0.0.1:{}", p.port))
        .unwrap_or_default();
    (endpoint, String::new())  // spec_json 永远为空
}
```

`gateway/mod.rs` 调用 `start_lsp_relay_supervisor` 时传入 pusher。

#### C3.5 启动时 codebase 注册路径保持现状

`startup/agent_init.rs:343-362` 当前从 `AgentHelloConfig.lsp_relay_endpoint` 读取 LSP relay 端点决定是否注册 codebase。**C3 不改这个**：

- 启动时若 LSP relay 已 ready → 注册 codebase
- 启动时若 LSP relay 未 ready → 不注册，等 SidecarEndpointUpdate 推送
- `register_external()` 检测到同名的会**替换**（不重复注册）

#### C3.6 `agent_tools.json` 默认启用 codebase

`{work_dir}/config/agent_tools.json` 当前 16 个工具，缺 codebase。C3 给 senior-engineer 包加上：

```json
{
  "name": "codebase",
  "enabled": true
}
```

**注意**：`enabled = true` 是**预期状态**——实际是否注册取决于 LSP relay 是否 ready。如果 LSP relay 还没起来，`register_external()` 不会调用，registry 里就没有 codebase，`enabled_entries` 里的 `codebase` 会被**静默跳过**（`registry.rs:55` 注释："Tools NOT in the registry but listed in `enabled_entries` are silently skipped"）。

这正是我们想要的：LSP relay 没起 → 工具面板不显示 codebase（"doomed calls" 避免）；LSP relay 起来 → push 触发 register_external → 工具面板出现 codebase。

#### C3 验收

- `cargo build --workspace` 通过
- `cargo test --workspace` 通过
- `ToolRegistry::register_external/unregister` 单测覆盖（含重名替换、空 registry、并发）
- `AgentCore::register_dynamic_tool` 单测覆盖（rebuild + broadcast）
- `cli.rs` `SidecarEndpointUpdate` 分支单测覆盖 LspRelay / Embed / Unspecified 三种 kind
- **端到端**：
  1. 启动 Gateway（LSP relay 还没 ready）
  2. 启动 senior-engineer agent → 工具面板**不显示** codebase
  3. 等待 LSP relay supervisor mark ready → agent 收到 SidecarEndpointUpdate → 工具面板**显示** codebase
  4. Kill LSP relay 进程 → reaper 推送 endpoint="" → 工具面板**移除** codebase
  5. embed 模型切换 → 推送 SidecarEndpointUpdate(Embed, new_endpoint, new_spec) → agent 重建 ONNX provider

---

### Phase C4：协议清理（待做）

> **目标**：彻底移除老 `embed_config_json` 字段和 `EmbeddingConfigUpdate` variant
> **用户已确认**："直接升级，项目还在开发中，没有兼容性需求"（06:22:13）

#### C4.1 移除 `RuntimeConfigUpdate.embed_config_json` 字段

`core/acowork-core/src/protocol.rs:1080` 删除字段定义 + 所有构造点。

构造点清单（待 C4 时搜索确认）：
- `core/acowork-gateway/src/ipc/global_push.rs` 的 `push_mcp_catalog()`（line 250-262）
- 其他 `RuntimeConfigUpdate { ..., embed_config_json: Some(...), ... }` 出现处

#### C4.2 移除 `GatewayResponse::EmbeddingConfigUpdate` variant

`core/acowork-core/src/protocol.rs:1134-1146` 删除整个 variant + `proto_bridge.rs` 对应转换代码 + `grpc/client.rs` 解码分支 + `cli.rs` 处理分支。

#### C4.3 移除 `push_embedding_config()` 函数

`core/acowork-gateway/src/ipc/global_push.rs:348-437` 删除函数 + `build_embed_sidecar_payload()` helper（已迁入 `push_sidecar_endpoint` 内部）。

#### C4.4 清理调用点

- `core/acowork-gateway/src/lifecycle/embed_supervisor.rs` 4 处 `push_embedding_config()` 调用——**已**在 C2 迁移到 `push_sidecar_endpoint`，C4 同步清理
- `core/acowork-gateway/src/http/embedding_api.rs` 1 处——同上
- 外部测试代码如果有调用 `push_embedding_config()` 需同步迁移或删除

#### C4.5 协议文档更新

`docs/adr/zh/ADR-030-...` 标注 C4 完成。
`docs/design/zh/12-tool-system.md` 等如有引用旧字段需同步更新。

#### C4 验收

- ✅ `cargo build --workspace` 通过（无 deprecation warning）
- ✅ `cargo test --workspace` 通过
- ✅ 搜索 `embed_config_json` 在 codebase 命中数为 0（ADR 文档中历史叙述除外）
- ✅ 搜索 `EmbeddingConfigUpdate` 在 codebase 命中数为 0（ADR 文档中历史叙述除外）
- ✅ 端到端：embed 模型切换 → `push_sidecar_endpoint(Embed, ...)` 推送 → runtime 收到后重建 provider 链

#### C4 兼容性影响

- **首次 AgentHello**：Runtime 仍能从 `AgentHelloResult.embed_endpoint` 拿到初始端点（这个字段不动）
- **运行中 embed 模型切换**：老 runtime（C4 之前的版本）**收不到推送**——但首次启动仍然能工作
- 这是用户接受的取舍（项目还在开发中，无兼容性需求）

---

## 关键设计决策

### D1：为什么用独立消息 `SidecarEndpointUpdate` 而不是 `RuntimeConfigUpdate` 内嵌字段？

| 维度 | 内嵌字段（旧） | 独立消息（新） |
|------|---------------|---------------|
| proto 表达力 | 每加一个 sidecar 都要加 N 个独立字段 | 加一个 enum 变体 + 复用 spec_json |
| 类型安全 | JSON 套 JSON 字段名拼写易错 | proto enum 强制类型 |
| 语义清晰 | `RuntimeConfigUpdate` 混杂 LLM 参数 + embed 端点 | 单一职责 |
| 扩展性 | 难加新 sidecar | 加 enum 变体即可 |
| 代价 | — | 多一个 proto message |

**选择独立消息**。旧 `embed_config_json` 字段在 C1/C2/C3 期间保留过渡期，C4 一次性清理。

### D2：为什么 `endpoint = ""` 表示"不可用"而不是 `Option<String>`？

proto 字段不能表达 `Option<String>` 的 None 语义。空字符串是天然的"无"标记，且 Runtime 端判断 `endpoint.is_empty()` 即可。spec_json 同样用空字符串表示"无 metadata"。

### D3：ToolRegistry 改为 `Arc<RwLock<Vec<...>>>` 是否影响性能？

`add_tool/remove_tool` 是低频操作（sidecar 状态变化最多每分钟几次），用 `RwLock` 没问题。`all()` 改为 `try_read` 快照，绝大多数情况下锁空闲，能拿到一致视图。`all_tools_snapshot()` 异步 API 用于必须保证一致性的场景。

### D4：启动时是否保留 codebase 启动期注册？

**保留**。原因：
- 改动最小，C3 的 `register_external` 自然覆盖启动后变化
- AgentHelloConfig 已经携带 lsp_relay_endpoint 字段（line 833）
- 即便 LSP relay 启动后立刻 crash，supervisor 会 push `endpoint = ""` 触发 `unregister`
- 启动期 + 动态注册**互补不冲突**：`register_external` 同名替换语义

### D5：为什么不废弃 `AgentHelloConfig.lsp_relay_endpoint`？

AgentHelloResult 是**握手时**的配置快照，SidecarEndpointUpdate 是**握手后**的推送更新，两者**不冲突**：
- Runtime 启动期用 AgentHelloConfig 决定初始状态
- Runtime 运行期用 SidecarEndpointUpdate 响应变化

`SidecarKind` 的扩展也不影响 AgentHelloConfig——它只承载 LSP relay endpoint 一个字段，未来加新 sidecar 只需在 `SidecarEndpointUpdate` 加 enum 变体。

### D6：L1 Readiness Barrier 已被否决，为什么？

用户 05:23:36 明确否决：
> "embed/lsp是进程启动初始化，延迟是秒级，后续可能还会有其他子进程引入，等所有进程ready，延迟不可接受"

正确做法：AgentHello 维持现状（snapshot 立即返回，不阻塞），后续 sidecar ready 由 pusher 异步补。L1 看起来"治本"但实际引入了新的全局启动延迟，每次加 sidecar 都会变。

### D7：embed 走 SidecarEndpointUpdate 后，老 runtime 兼容性如何？

- C2/C3 期间：`push_embedding_config()` deprecated wrapper 让老 runtime（仍读 `embed_config_json`）**也**能收到推送——双通道共存
- C4 之后：老 runtime 失去运行中 embed config push，但首次 AgentHello 仍能拿到
- 这是用户接受的取舍（06:22:13："直接升级，项目还在开发中，没有兼容性需求"）

---

## 风险与缓解

| 风险 | 影响 | 缓解 |
|------|------|------|
| ToolRegistry 改为 RwLock 后 `all()` 锁竞争 | LLM 工具列表构建在主路径上 | `try_read` 不阻塞；锁空闲时是直接指针拷贝 |
| embed supervisor 迁移到 push_sidecar_endpoint 后语义有变 | Runtime 端收到重复 push | Runtime 端 `register_external` 用 name 去重；embed provider 链用 endpoint 变化检测 |
| LSP relay 启动期和 push 期重复注册 codebase | 工具面板闪烁 | `register_external` 同名替换；首次 push 时如果已注册则 skip |
| C4 移除 `embed_config_json` 字段后老 runtime 静默失败 | 升级未完成用户的 embed 重启不感知 | 文档明确升级路径；runtime binary 单独发版，不和 gateway 强耦合 |
| LSP relay supervisor 多处状态变化点容易漏 push | 部分边界 case 不推送 | C3 验收时跑 supervisor 重启 / kill -9 / 端口冲突 三个场景 |

---

## 决策记录

- 2026-07-08 草案创建。Phase C1（协议层）已完成（HEAD），C2/C3/C4 待实施。
- 2026-07-08 用户确认走法 B（C1→C2→C3→C4，4 个独立 buildable commit）。
- 2026-07-08 用户确认 embed 完全迁移到 SidecarEndpointUpdate（选项 B，老通道 C4 清理）。
- 2026-07-08 用户确认前端不在本次 4 commit 范围。
- 2026-07-08 用户否决 L1 Readiness Barrier，确认 AgentHello 维持现状 + pusher 异步补。
