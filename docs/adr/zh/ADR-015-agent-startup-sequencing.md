# ADR-015: Agent 启动时序重构 — 从异步竞争到分阶段就绪

**状态**：草案（待实施）
**日期**：2026-06-19
**决策者**：架构讨论
**影响范围**：

- `core/acowork-runtime/src/cli.rs`（async_main 启动流程重排）
- `core/acowork-runtime/src/agent/session/session_manager.rs`（SessionState 装配集中化）
- `core/acowork-runtime/src/agent/session/session_task.rs`（退化为被动 handler）
- `core/acowork-runtime/src/agent/loop_session.rs`（emit_session_state 调用时机）
- `core/acowork-gateway/src/http/`（新增 session state pull 接口）
- `apps/acowork-desktop/src/lib/agent-start.ts`（syncAgentUI 增加 fetchSessionState）
- `apps/acowork-desktop/src/stores/chatStore.ts`（首次进入会话时主动拉取状态）

---

## 背景

前端会话状态面板（ResultsPanel）需要在 Agent 启动时立即显示该会话的当前 `model`、`provider`、`reasoning_effort`（思考强度）、`temperature`、`workspace_id`、`ratio`（字符/token 比）等信息。然而在当前实现下，**冷启动后第一次进入会话时，Thinking Level 一栏长期显示 `off`，需要手动切换模型才能恢复正常**。多次定位修复均未根除——根因不在某个具体字段的初始化逻辑，而在**整个 Agent 启动时序的设计**。

### 问题 1：SessionTask 的初始 emit 早于 chunk_relay 与前端 WebSocket 连接

当前时序（`cli.rs::async_main`）：

```text
T=0    AgentHello → AgentHelloResult                    主线程
T=10   AgentCore::new + global_provider_list 注入        主线程
T=20   SessionManager::create_session_with_id_and_conversation
         └─ tokio::spawn(SessionTask::run)              ⚡ 异步分叉点
              T=20.1  SessionTask 内部初始化 reasoning_effort
              T=20.2  SessionTask::emit_session_state()  ← 第一次推送
                       └─ chunk_tx.try_send(SessionStateChanged)
                          （此时 chunk_rx 没人 recv，事件被 buffer）
              T=20.3  SessionTask 进入 inbound 循环
T=21   主线程: route_model_switch / SetWorkDir / UpdateRuntimeConfig
        每个消息都可能触发 SessionTask 再次 emit_session_state
T=40   AgentReady → Gateway 设置 ready=true
T=41   spawn chunk_relay → 开始 chunk_rx.recv()
        └─ 一次性把 buffer 中的多个 SessionStateChanged 倾泻到 outbound
T=50   run_gateway_loop（"Gateway message loop started"）
T≥500  Desktop App waitForAgentReady 轮询发现 ready=true
T≥501  Desktop App connectStream() 建立 WebSocket
        ⚠ 此时 chunk_relay 早已把初始 session_state_changed intent
           发给 Gateway；如果 Gateway 不缓存最近一次状态，前端永远收不到。
```

**核心症状**：前端能否拿到初始状态完全依赖 WebSocket 连接时间与 chunk_relay 起步时间的相对关系——这是一个不应当存在的竞争条件。

### 问题 2：AgentReady 的语义混乱

当前 AgentReady 在 `cli.rs:1269` (Step 10) 发送，此时：

- ✅ AgentCore 已就绪
- ✅ SessionManager 已创建
- ⚠ SessionTask **已 spawn 但状态未稳定**（主线程还会继续给它发 ModelSwitch / SetWorkDir / UpdateRuntimeConfig）
- ❌ chunk_relay **还未 spawn**（在 Step 11 才 spawn）
- ❌ MCP 还在后台连接（最长 30s）
- ❌ run_gateway_loop **还未启动**（Step 12）

`ready` 的实际语义是"主线程跑完了同步初始化的一部分"，而不是"Runtime 完全就绪、可以接收并响应任何前端请求"。前端拿到 `ready=true` 后立即建立 WebSocket，就掉进了上面问题 1 的竞争窗口。

### 问题 3：per-session 状态在多处并行写入

`reasoning_effort` 的初始化逻辑在四个不同位置都存在：

| 位置                                               | 时机                     |
| -------------------------------------------------- | ------------------------ |
| `SessionManager::create_session_with_id_*`（L312） | session 创建时           |
| `SessionTask::run` 启动初始化（L468）              | spawn 后立即             |
| `SessionTask` 处理 `ProviderListUpdated`（L1073）  | provider list 异步更新时 |
| `SessionTask` 处理 ModelSwitch（L1083、L1182）     | 模型切换时               |

四份代码做近似相同的事（从 model capabilities 读 default → 解析 → 设置到 SessionState），但执行顺序受异步消息驱动，难以保证最终一致性。`temperature` 的处理同样散落在 SessionState、AgentCore 与 runtime_overrides 三处。

### 问题 4：前端依赖 push 拿初始状态

当前架构假设"SessionTask 启动时 emit 一次状态，前端通过 WebSocket 收到"。这种设计有两个根本缺陷：

1. push 是 fire-and-forget 的——发送方无法确认接收方已就绪；
2. 多 session 场景下（用户切换 tab）需要显式触发 emit 才能拿到该 session 的当前状态，逻辑变得冗余。

### 问题 5：临时修复的代价

在不重构启动时序的前提下，已有的修复尝试包括：在 SessionTask::run 启动时主动初始化 reasoning_effort、引入 `SessionMessage::ProviderListUpdated` 广播事件、在 `emit_session_state` 中加入 lazy-init（被否决并回滚）、添加大量 tracing 日志定位丢失的事件。每一处都在治标——核心时序问题不解决，下一个新增的 per-session 字段会重蹈覆辙。

---

## 决策

将 Agent 启动时序重构为**两阶段同步初始化 + 末尾 AgentReady + 前端主动 pull** 的模式。SessionTask 不再承担"启动时 emit 初始状态"的职责，前端通过新增的 `GET /api/agents/{agent_id}/sessions/{session_id}/state` 接口主动拉取快照。

### 核心原则

1. **per-agent 与 per-session 严格分阶段** — 阶段 A 完成所有跨 session 共享的初始化（provider list、key vault、tools、embedding、memory store），阶段 B 才创建 session。
2. **SessionState 在主线程同步装配完整** — `reasoning_effort` / `temperature` / workspace / history 全部在 SessionManager 中、`tokio::spawn` 之前完成，spawn 出去的 SessionTask 接手时拿到的就是完整状态。
3. **SessionTask 退化为被动 message handler** — 删除"启动时初始化 + emit"逻辑，仅处理运行时事件（user message、ModelSwitch、debug）。
4. **AgentReady 是真就绪信号** — 在所有同步初始化完成、chunk_relay 已 spawn、run_gateway_loop 即将进入消息循环之前发送，语义为"Runtime 已完全就绪"。
5. **快照走 pull、变化走 push** — 初始状态前端主动 pull；运行时变化（streaming、status 转换、ModelSwitch 后的新状态）继续走 chunk_relay push。

### 时序对比

#### 重构前（异步竞争）

```text
async_main 主线程:
  AgentHello → 装配 → SessionManager::create_session
    └─ tokio::spawn(SessionTask::run) ⚡ 分叉
                     ├─ 初始化 reasoning_effort
                     ├─ emit_session_state ← buffer 到 chunk_tx
                     └─ 进入 inbound loop
  → route_model_switch / SetWorkDir / UpdateRuntimeConfig
  → AgentReady（语义模糊：实际未就绪）
  → spawn chunk_relay
  → run_gateway_loop
```

#### 重构后（分阶段就绪）

```text
async_main 主线程（全部同步）:
  ── 阶段 A: per-agent 初始化 ──
    AgentHello → AgentHelloResult
    构建 system_prompt / SkillRegistry / tools / embedding
    AgentCore::new + 注入 global_provider_list / key_vault / memory_session
    init_memory_store
    SessionManager::new

  ── 阶段 B: per-session 初始化（同步，不 spawn）──
    加载 conversation（resume 最新 / 创建新 session）
    校验 provider/model + fallback
    构建完整 SessionState（model, provider, reasoning_effort,
                           temperature, workspace_id, history, MCP tools）
    持久化 SessionState header 到 JSONL（仅当有 fallback 修正时）

  ── 阶段 C: 启动子系统 ──
    tokio::spawn(SessionTask::run)        ← 被动 handler，不再 emit
    spawn(chunk_relay)                     ← 此时通道两端齐备
    spawn(MCP 后台连接)                    ← 后台异步，不阻塞

  ── 阶段 D: 通告就绪 ──
    AgentReady → Gateway: agent.ready = true
    run_gateway_loop("ready to receive inbound messages")

Desktop App:
  waitForAgentReady（轮询） → connectStream（WS）
                            → fetchSessionState（HTTP pull，完整快照）
                            → 后续变化通过 WS push 增量更新
```

---

## 实施步骤

总计 7 个 Phase。每个 Phase 独立可编译、可手动测试，不破坏现有功能。建议每个 Phase 单独提交一个 commit，便于回滚。

### Phase 依赖关系

Phase 0 → Phase 1 → Phase 2 → Phase 3 → Phase 4 → Phase 5 → Phase 6

其中 Phase 0-3 **必须严格顺序实施**，不可并行开发。Phase 4（pull 接口）和 Phase 5（前端集成）可在 Phase 3 完成后并行。Phase 6 必须最后执行。

### Phase 0：`async_main` 结构化拆分为 Phase 函数

**目标**：将 3,600+ 行的超大函数 `async_main` 拆分为清晰的 Phase 编排器 + 独立 Phase 函数，使启动流程一目了然。

**改动文件**：
- `core/acowork-runtime/src/cli.rs`（拆分为编排器 + phase 函数）
- 可选：新增 `core/acowork-runtime/src/startup/` 模块目录

**核心设计**：

引入 `AgentBootContext` 结构体，作为 Phase 之间的数据传递载体，替代散落的十几个局部变量：

```rust
/// Intermediate context produced by Phase A, consumed by subsequent phases.
struct AgentBootContext {
    package: LoadedPackage,
    grpc_client: GatewayGrpcClient,
    hello_config: AgentHelloConfig,
    provider: Arc<dyn LLMProvider>,
    embedding: Arc<dyn EmbeddingProvider>,
    tool_registry: ToolRegistry,
    skill_registry: SkillRegistry,
    chunk_tx: ChunkSender,
    chunk_rx: Option<ChunkReceiver>,
    // ... other per-agent resources
}
```

重构后的 `async_main` 变为 ~20 行的编排器：

```rust
async fn async_main(config: RuntimeConfig, ...) -> Result<()> {
    // Phase A: per-agent resources
    let agent_ctx = phase_a_init_agent(&config).await?;

    // Phase B: per-session state (sync assembly)
    let session_ctx = phase_b_init_session(&agent_ctx, &config).await?;

    // Phase C: spawn subsystems
    let subsystems = phase_c_spawn_subsystems(&agent_ctx, &session_ctx).await?;

    // Phase D: announce ready & enter loop
    phase_d_run(&agent_ctx, &session_ctx, subsystems).await
}
```

每个 Phase 函数 200-400 行，职责单一、独立可测试。可选方案是将 Phase 函数放入独立的 `startup/` 子模块：

```
src/
├── cli.rs              # 仅剩 arg parsing + async_main 编排（~100 行）
├── startup/
│   ├── mod.rs          # re-exports
│   ├── context.rs      # AgentBootContext 定义
│   ├── agent_init.rs   # Phase A
│   ├── session_init.rs # Phase B
│   ├── subsystems.rs   # Phase C (chunk_relay, MCP)
│   └── gateway_loop.rs # Phase D
```

**对 Standalone 模式的处理**：Phase A/B 对 Gateway 和 Standalone 两种模式通用，Phase C/D 按模式分叉。Standalone 模式在 Phase C 不 spawn chunk_relay，Phase D 直接进入 `run_chat_loop`。

**验证**：`cargo build -p acowork-runtime`；`async_main` 函数体 < 50 行；各 Phase 函数独立可编译。

---

### Phase 1：抽取 SessionState 装配逻辑到 SessionManager 辅助函数

**目标**：把散落在 `create_session_with_id_and_conversation`、`SessionTask::run` 启动初始化、`ProviderListUpdated` handler、`ModelSwitch` handler 中的"装配 SessionState"逻辑集中到 `SessionManager` 的一个辅助函数。Phase 1 只抽取，不改时序。

**改动文件**：`core/acowork-runtime/src/agent/session/session_manager.rs`

**新增函数签名**：

```rust
/// Build a fully-initialized SessionState for a new or resumed session.
/// All per-session fields are set synchronously before this returns.
/// Caller must hold an Arc<AgentCore> with global_provider_list populated.
fn build_initial_session_state(
    &self,
    conversation: Option<&ConversationSession>,
) -> SessionState
```

**搬入此函数的逻辑**：

| 当前来源                            | 行为                                                                |
| ----------------------------------- | ------------------------------------------------------------------- |
| `create_session_with_id_*` L287-316 | new SessionState + set_model/provider                               |
| 同上 L301-302                       | history_mut().set_max_tokens(context_trim_budget)                   |
| 同上 L306-312                       | 从 model capabilities 解析 default_reasoning_effort 并 set          |
| 同上（缺失，需要新增）              | 从 AgentCore 读取或 SessionManager 缓存的 temperature override，set |

**验证**：`cargo build -p acowork-runtime` + `cargo test -p acowork-runtime --lib session_manager` 全绿；agent 冷启动行为与重构前一致（仍有 reasoning_effort=off 的 bug，但不会更糟）。

### Phase 2：SessionTask::run 删除启动时的初始化与 emit_session_state

**目标**：SessionTask 退化为纯被动 handler。它接到的 SessionState 已经由主线程装配完整，无需自己再做。

**改动文件**：`core/acowork-runtime/src/agent/session/session_task.rs`

**删除的代码**：

- L449-469：从 model capabilities 初始化 reasoning_effort 的整段逻辑
- L471-481：启动时调用 `agent_loop.emit_session_state()` 的整段逻辑
- L1073、L1083、L1182：`ProviderListUpdated` 和 `ModelSwitch` 中重复的 reasoning_effort 初始化（保留 ModelSwitch 中"切换后从新模型 capabilities 重置"的逻辑，但收敛为调用一个共享的 `apply_model_defaults` 私有方法）

**保留的 emit 触发点**：

- 每次 `transition_status` 后（`loop_session.rs` 内）
- ModelSwitch 完成后（手动调用一次 `emit_session_state`）
- SetWorkDir 完成后

> 这些 emit 都发生在 chunk_relay 已就绪之后，因此事件能正常流到前端。

**验证**：`cargo build -p acowork-runtime`；启动 agent，观察日志中 SessionTask 不再打印 `SessionTask: initializing reasoning_effort`；状态变化（如发消息后 streaming/idle 切换）的 session_state_changed 仍能被前端收到。

### Phase 3：cli.rs 重排启动顺序，AgentReady 移到末尾

**目标**：把 `async_main` 拆成清晰的四阶段，按 A→B→C→D 顺序执行，最后才发 AgentReady。

**改动文件**：`core/acowork-runtime/src/cli.rs`

**具体调整**（按当前行号映射）：

| 阶段     | 当前位置        | 新位置 / 调整                                                                          |
| -------- | --------------- | -------------------------------------------------------------------------------------- |
| 阶段 A   | L313-961        | 保持不变（package、hello、provider、tools、embedding、AgentCore、SessionManager::new） |
| 阶段 B   | L769-851 + L993 | 把 conversation 加载、provider/model 校验、create_session 集中在阶段 A 之后            |
| 阶段 B+  | L1054-1131      | workspace_context、agent_config overrides 在 create_session 之前**作为入参**传进去     |
| 阶段 B+  | L1009-1033      | 删除：route_model_switch（因为 Phase 1 后 SessionState 装配时已用正确 provider）       |
| 阶段 C   | L1296-1660      | 不变：spawn chunk_relay                                                                |
| 阶段 C   | L1187-1239      | 不变：spawn MCP 后台连接                                                               |
| 阶段 D   | L1269-1294      | **移到 L1660 之后**：AgentReady 发送                                                   |
| 阶段 D   | L1544-1730      | 不变：进入 run_gateway_loop                                                            |

**关键代码变更**：

```rust
// 新增：在阶段 B 之前，从 agent_config.json 读取 overrides，
// 通过 SessionManager 缓存供 build_initial_session_state 使用
let agent_overrides = load_agent_config(work_dir_path).unwrap_or_default().unwrap_or_default();
session_manager.set_runtime_overrides_cache(agent_overrides.clone());

// 阶段 B：create_session 时传入 overrides 与 workspace_context
let initial_session_id = session_manager
    .create_session_with_id_and_conversation(sid.clone(), conversation)
    .await?;

// 阶段 C：spawn chunk_relay
let chunk_relay = tokio::spawn(...);

// 阶段 D：所有就绪后才通告 ready
client.outbound_sender().send(AgentReady{...}).await?;
run_gateway_loop(...).await
```

**验证**：日志中 `AgentReady sent to Gateway` 出现的时间晚于 `Chunk relay started`；前端 `waitForAgentReady` 等待时间略增（约 100-200ms，可接受）；冷启动后无任何 session_state_changed 事件被丢弃。

### Phase 4：新增 Runtime → Gateway → 前端的 session state pull 接口

**目标**：让前端能在 WS 连上后主动拉一份完整 SessionState 快照。

**改动文件**：

- `core/acowork-runtime/src/cli.rs`（gateway_recv 处理新的 `GetSessionState` 请求）
- `core/acowork-runtime/src/agent/session/session_manager.rs`（新增 `snapshot_session_state(&self, session_id: &str) -> Option<SessionStateSnapshot>`）
- `core/acowork-core/src/proto/`（新增 `GetSessionStateRequest` / `GetSessionStateResponse` proto 消息）
- `core/acowork-gateway/src/http/chat.rs` 或新文件 `session_state.rs`（新增 `GET /api/agents/{agent_id}/sessions/{session_id}/state` HTTP 路由）
- `core/acowork-gateway/src/grpc/dispatch.rs`（路由 GetSessionState 到对应 Runtime）

**新增数据结构**：

```rust
#[derive(Serialize)]
pub struct SessionStateSnapshot {
    pub session_id: String,
    pub status: SessionStatus,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub workspace_id: Option<String>,
    pub ratio: Option<f64>,
    pub reasoning_effort: Option<String>,
    pub temperature: Option<f32>,
}
```

**关键实现**：`SessionManager::snapshot_session_state` 直接读取 `SessionHandle` 内持有的 SessionState 快照。**注意**：SessionTask 也会读写 SessionState，因此装配阶段（主线程）和 SessionTask 必须共享同一份 SessionState 的可见视图。

**首选方案**：将 snapshot 字段（model/provider/reasoning_effort/temperature/workspace_id，仅 ~10 个轻量字段）拆出来作为 `Arc<RwLock<SessionStateSnapshot>>`，由 SessionTask 在每次状态变化时同步写入。理由：SessionState 是大结构体（含 history、tool results），对其整体加锁引入不必要的锁竞争；独立的 `SessionStateSnapshot` 改动面小、性能好，足以满足 pull 接口的读取需求。

**备选方案**：如果后续发现 snapshot 字段无法满足需求，再考虑把整个 `SessionState` 改为 `Arc<RwLock<SessionState>>` 共享所有权——但需注意对大结构体整体加锁会引入不必要的锁竞争。

**验证**：`curl http://127.0.0.1:19876/api/agents/com.acowork.senior-engineer/sessions/{sid}/state` 返回完整 JSON 快照；多次调用返回最新值；不存在的 session 返回 404。

### Phase 5：Desktop App 接入 fetchSessionState

**目标**：前端在 `syncAgentUI` 中拿到 `agent.ready=true` 后，先建立 WebSocket，再主动拉取初始 session 状态。

**改动文件**：

- `apps/acowork-desktop/src/lib/agent-start.ts`（新增 `fetchInitialSessionState`）
- `apps/acowork-desktop/src/stores/chatStore.ts`（新增 `fetchSessionState(agentId, sessionId)` action，把响应映射到 `SessionChatState`）
- `apps/acowork-desktop/src/stores/sessionStore.ts`（如有需要：在切换 session tab 时也调用 fetchSessionState）

**关键代码变更**：

```typescript
// agent-start.ts
export async function syncAgentUI(agentId: string) {
  await useAgentStore.getState().waitForAgentReady(agentId);
  useChatStore.getState().connectStream(agentId, getGatewayUrl());

  // NEW: pull initial session state for the active session
  const activeSid = useChatStore.getState().getActiveSessionId(agentId);
  if (activeSid) {
    await useChatStore.getState().fetchSessionState(agentId, activeSid);
  }

  useWorkspaceStore.getState().fetchWorkspaces(agentId);
  emitAgentConfigRefresh(agentId);
}
```

**验证**：冷启动 agent，进入会话立即看到 Thinking Level 显示正确值（不再是 `off`）；切换 session tab 时也能立即拿到该 session 的状态；网络监控中能看到一次 `GET /api/agents/.../state` 请求。

### Phase 6：清理临时修复与冗余日志

**目标**：删除前几次"治标"修复留下的代码。

**改动文件**：

- `core/acowork-runtime/src/agent/session/session_task.rs`（删除 ProviderListUpdated 处理中冗余的 reasoning_effort 初始化日志）
- `core/acowork-runtime/src/agent/session/session_manager.rs`（评估 `update_global_provider_list` 中的 broadcast 是否仍需要——如果运行时不再热更新 provider list，可删除）
- `core/acowork-runtime/src/agent/loop_session.rs`（删除 try_send 失败时的 warn 日志或降级为 debug——SessionTask 已不再做启动 emit，try_send 失败主要发生在 channel 关闭时，正常路径不会出现）
- **保留** `SessionMessage::ProviderListUpdated` 消息变体——它在运行时是必须的：前端修改 provider list（如添加/删除 API key）需要实时通知所有活跃的 SessionTask 更新可用模型列表。Phase 6 不删除此消息变体，只需清理其中**冗余的 reasoning_effort 重新初始化逻辑**——改为仅更新可用模型列表，不重置当前 session 的 reasoning_effort

**验证**：`cargo clippy --all-targets -- -D warnings` 全绿；`cargo test` 全绿；冷启动 + 模型切换 + 多 session 切换三个场景全部正常。

---

## 否决方案

### A. 在 Gateway 端缓存最近一次 session_state

让 Gateway 在收到 `session_state_changed` intent 时缓存最新值，前端 WebSocket 连上时立即把缓存的状态推一次。

**否决原因**：

- 增加了 Gateway 的有状态依赖，违反 Gateway 作为"无状态路由"的设计（参见 `module-design/zh/03-gateway.md`）；
- 多 session 时 Gateway 需要为每个 session 维护一份缓存，与 SessionManager 的 SessionState 形成数据冗余；
- 仍然不能解决"主线程在 SessionTask 启动后继续推消息导致状态变化"的根本问题；
- 没有解决多 session tab 切换时如何拿到非 active session 状态的问题。

### B. AgentReady payload 携带初始 SessionState 快照

扩展 AgentReady proto，让它直接携带 `initial_session_id` 和完整 `SessionState` 快照，Gateway 透传到 HTTP `/api/agents` 响应中。

**否决原因**：

- AgentReady 从"标志"变成"携带数据"，语义变重；
- 多 session 时只能携带一个 initial session 的快照，其他 session 仍要 pull——既然如此不如统一走 pull；
- 与现有的 `/api/agents` 列表接口耦合，改动面大于本 ADR 的方案；
- pull 方案的额外延迟 < 50ms，用户无感。

### C. SessionTask 自带"已就绪"信号 + 主线程等待

让 SessionTask 在 `run()` 中通过 oneshot channel 通知主线程"我已就绪"，主线程等到该信号才发 AgentReady。

**否决原因**：

- SessionTask 内部"就绪"的定义模糊（emit 完初始状态？进入 inbound loop？）；
- 仍然需要前端 pull 接口来支持多 session 场景；
- 增加跨任务同步原语（oneshot），却没有把 SessionState 装配真正搬到主线程，问题没解决；
- 与本 ADR 的核心原则"装配在主线程同步完成"相悖。

### D. 不重构、把 reasoning_effort 默认值硬编码为 Medium

最简化的兜底方案：直接在 `SessionState::new` 里把 `reasoning_effort` 字段默认设为 `Some(ReasoningEffort::Medium)`，永远不依赖 model capabilities。

**否决原因**：

- 永久丢失"模型自带的推荐 effort"语义（如某些推理模型默认应当是 High，弱模型默认应当是 Off）；
- 当 model capabilities 中明确规定了 default_reasoning_effort 时仍需读取——bug 仍在；
- 不解决 temperature、workspace_context 等其他 per-session 字段的同类问题；
- 是治标不治本的典型，违背用户明确指出的"梳理时序"诉求。

---

## 风险与回滚

### 风险 1：SessionState 所有权重构（Phase 4）改动面较大

`SessionState` 当前由 `SessionTask` 独占持有，要让主线程的 `snapshot_session_state` 也能读取，需要改成 `Arc<RwLock<SessionState>>` 或类似共享所有权。如果直接重构 SessionState 风险过高，**回退方案**：在 `SessionHandle` 中维护一份独立的 `Arc<RwLock<SessionStateSnapshot>>`（仅含 snapshot 字段，~10 个字段），由 SessionTask 在每次 emit_session_state 时同步写入。

### 风险 2：现有 emit 调用点遗漏

Phase 2 删除 SessionTask 启动时的 emit 后，必须确保所有"运行时状态变化"的 emit 调用点仍然存在。需要在 PR 中列出所有 `emit_session_state` 调用点，逐个 review。

### 风险 3：Phase 3 调整后 workspace_context / runtime_overrides 装配逻辑改动

当前 workspace_context / runtime_overrides 是在 session 创建之后通过广播消息送达的，重构后改为创建时入参。需要仔细 review `update_session_workspace_context` 和 `apply_runtime_config_override` 这两个函数的调用方，确保不会遗漏初始化路径。

### 回滚策略

每个 Phase 是独立 commit，可逐个 revert。最危险的是 Phase 3（cli.rs 重排）和 Phase 4（snapshot 接口），这两个 Phase 应当各自单独成 PR，独立 review 和测试。

---

## 补充设计约束

### 约束 1：`SessionManager::create_session_complete` 统一入口

Phase B 的核心逻辑应下沉到 `SessionManager` 内部，提供 `create_session_complete()` 方法，一次性完成"加载 conversation → 校验 provider/model → 装配完整 SessionState → 注册 handle"的全流程。这样：

- `cli.rs` 的 Phase B 只需一行调用
- 后续用户创建新 session 也走同一条路径，避免"初始 session 走 cli.rs 路径" vs "后续 session 走 SessionManager 路径"的代码分裂
- `route_model_switch`、`update_session_workspace_context`、`apply_runtime_config_override` 收编为 `create_session_complete` 的入参或内部步骤

### 约束 2：tracing span 标记各 Phase

每个 Phase 函数入口使用 `info_span!` 标记，使日志天然呈现阶段划分和耗时：

```rust
async fn phase_a_init_agent(config: &RuntimeConfig) -> Result<AgentBootContext> {
    let _span = tracing::info_span!("startup_phase_a").entered();
    // ...
}
```

### 约束 3：Phase B 超时保护

Phase B 涉及磁盘 I/O（conversation 加载）和可能的网络验证（provider 校验），应设置合理的超时（建议 10s），避免阻塞整个启动流程。超时后应 fallback 到默认状态而非 panic。

### 约束 4：集成测试覆盖

重构前应新增端到端集成测试，验证"冷启动 → 前端拿到正确 session state"的完整链路。建议在 `core/tests/` 下新增 `startup_sequencing_test.rs`，至少覆盖：

- 冷启动后 session state snapshot 包含正确的 reasoning_effort
- AgentReady 时间戳晚于 chunk_relay spawn 时间戳
- pull 接口返回与 push 事件一致的状态

---

## 验收标准

- [ ] 冷启动 agent，第一次进入会话面板，Thinking Level 立即显示正确值（不再是 `off`）；
- [ ] 冷启动 agent，第一次进入会话面板，Temperature 立即显示正确值（默认 0.70 或模型 override）；
- [ ] 切换模型（ModelSwitch）后，状态面板的所有字段在 1s 内更新；
- [ ] 切换 session tab，状态面板立即反映该 session 的真实状态；
- [ ] 后端日志中：`AgentReady sent to Gateway` 时间戳 > `Chunk relay started` > `SessionManager: created new session`；
- [ ] 后端日志中：不再出现 `SessionStateChanged event dropped` 警告；
- [ ] `cargo clippy --all-targets -- -D warnings` 全绿；
- [ ] `cargo test` 全绿；
- [ ] `npx tsc --noEmit`（前端）全绿；
- [ ] `async_main` 函数体 < 50 行（仅含 Phase 编排调用）；
- [ ] 各 Phase 函数均可独立编译和单元测试；
- [ ] 启动日志中能看到 `startup_phase_a`、`startup_phase_b`、`startup_phase_c`、`startup_phase_d` 四个 tracing span 及各自耗时。

---

## 参考

- 当前实现：
  - `core/acowork-runtime/src/cli.rs`（async_main，4020 行）
  - `core/acowork-runtime/src/agent/session/session_task.rs`（SessionTask::run）
  - `core/acowork-runtime/src/agent/session/session_manager.rs`（create_session_with_id_and_conversation）
  - `core/acowork-runtime/src/agent/loop_session.rs`（emit_session_state）
- 前序 ADR：
  - ADR-012（Per-session model isolation）— 模型从全局变为 per-session 的设计
  - ADR-013（Debug Observer Pipeline）— 类似的"主线程同步装配 + spawn 后被动消费"模式
  - ADR-014（AgentLoop 模块拆分）— 分阶段重构的实施方法论
- 相关日志（用于 root cause 分析）：
  - `C:\Users\Administrator\.acowork\acowork-gateway\config\packages\com.acowork.senior-engineer\workspace\logs\20260618_224759.log`
- 模块设计文档：
  - `docs/module-design/zh/02-runtime.md`（Runtime 模块设计）
  - `docs/module-design/zh/03-gateway.md`（Gateway 无状态路由原则）
