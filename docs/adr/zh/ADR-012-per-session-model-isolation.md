# ADR-012: Per-Session Model/Provider 隔离

**状态**：提议  
**日期**：2026-05-29  
**决策者**：架构讨论  
**影响范围**：`chatStore.ts`, `ChatPanel.tsx`, `session_manager.rs`, `session_task.rs`, `cli.rs`, `conversation.rs`, `chat.rs`（Gateway）

---

## 背景

当前 Model/Provider 存储在前端 `AgentState`（agent 级别）和后端 `SessionManagerConfig.override_model`（agent 级别），`model_switch` 通过 `broadcast()` 推送给所有 session，导致切换一个 session 的 model 会影响同 agent 下所有 session。

参考 `workspace_id` 的成功模式——它存储在 `SessionMetadata`（JSONL 首行），每个 session 独立持久化。Model/Provider 应采用相同的 per-session 模式。

### 要删除的废代码

**前端：**
- `AgentState.model`, `AgentState.provider` — 删除
- `ChatStore.currentModel`, `ChatStore.currentProvider` — 删除
- `ChatStore.agentModels` — 删除
- `setAvailableModels` 中回退 currentModel 的逻辑 — 删除

**后端：**
- `SessionManagerConfig.override_model` — 删除
- `session_manager.update_model_override()` — 删除
- `save_agent_model()`, `load_agent_model()` — 删除
- `AGENT_MODEL_FILE` 常量, `AgentModelEntry` 结构体 — 删除
- `cli.rs` 中所有读写 `agent_model.json` 的代码 — 删除
- `SessionTask::new()` 的 `override_model` 参数 — 删除
- 启动时从 `AgentHelloResult` cache model 的代码块 — 删除
- `QueryConfig` 响应中从 `load_agent_model` 读取 model 的代码 — 删除

## 决策

### 数据模型

#### 前端

**`SessionChatState`** 新增 model/provider 字段——这是 model 信息的**唯一**存储位置：

```typescript
interface SessionChatState {
  // ... existing fields
  model: string | null;     // per-session model（唯一来源）
  provider: string | null;  // per-session provider（唯一来源）
}
```

**删除** `AgentState.model`, `AgentState.provider`, `ChatStore.currentModel`, `ChatStore.currentProvider`, `ChatStore.agentModels`。

#### 后端

**`SessionMetadata`**（JSONL 首行）新增 model/provider 字段，与 `workspace_id` 同模式：

```rust
pub struct SessionMetadata {
    // ... existing fields
    pub model: Option<String>,      // per-session model
    pub provider: Option<String>,   // per-session provider
}
```

**`SessionState`**（内存）新增 model/provider 字段：

```rust
pub struct SessionState {
    // ... existing fields
    pub model: Option<String>,
    pub provider: Option<String>,
}
```

**删除** `SessionManagerConfig.override_model`, `AgentModelEntry`, `AGENT_MODEL_FILE`, `save_agent_model()`, `load_agent_model()`。

### 前端变更

#### 1. setCurrentModel → per-session

写入目标改为 session 级别，同时从 WS 消息携带 `session_id`：

```typescript
setCurrentModel: (model: string, provider: string, agentId: string) => {
  const sessionId = getAgentState(get(), agentId).activeSessionId;
  if (!sessionId) return;

  set((state) => updateSessionState(state, agentId, sessionId, { model, provider }));

  const ws = get().wsMap[agentId];
  if (ws?.readyState === WebSocket.OPEN) {
    ws.send(JSON.stringify({
      type: "model_switch",
      model, provider, agentId,
      session_id: sessionId,
    }));
  }
}
```

#### 2. UI 从 session 级别读取

```typescript
// ChatPanel.tsx
const currentModel = sessionState?.model ?? null;
const currentProvider = sessionState?.provider ?? null;
```

无冗余读取，无 fallback 链。

#### 3. `activateSession` 不再同步 model

`activateSession` 只更新 `activeSessionId`。UI 通过 `sessionState` selector 自动重新计算——`sessionState` 本身就是基于 `activeSessionId` 选中的，切换后自然读到目标 session 的 model。

```typescript
activateSession: (agentId: string, sessionId: string) => {
  set((state) => {
    // 只更新 activeSessionId 和 openSessionIds
    // model/provider 由 UI 层通过 sessionState 自动读取
    const patches: Partial<AgentState> = { activeSessionId: sessionId };
    // ...
    return { ...evictResult }; // 不再同步 currentModel/currentProvider
  });
}
```

#### 4. `model_confirmed` 事件更新 session 级别

```typescript
case "model_confirmed": {
  const sessionId = getAgentState(state, confirmedAgentId).activeSessionId;
  if (sessionId) {
    return updateSessionState(state, confirmedAgentId, sessionId, {
      model: confirmedModel,
      provider: confirmedProvider ?? "",
    });
  }
}
```

#### 5. `setAvailableModels` 简化

不再有 fallback 逻辑，只设置 available list：

```typescript
setAvailableModels: (models: ModelEntry[]) => {
  set({ availableModels: models });
}
```

#### 6. 新 session 创建时 model 初始化

创建新 session 时，model 初始值为 `null`，等 `model_confirmed` 事件或用户手动选择后写入：

```typescript
// sessionStore.ts createSession()
createSessionEntry: (agentId, sessionId) => {
  set((state) => updateSessionState(state, agentId, sessionId, {
    model: null,  // 初始 null
    provider: null,
  }));
}
```

### 后端变更

#### 1. 新 session model 初始化

不再从 `agent_model.json` 加载。新 session 的初始 model 直接使用 manifest `suggested_model`：

```rust
// session_manager.rs create_session()
let session_state = SessionState::new(
    self.config.history_max_tokens,
    self.config.per_session_budget.clone(),
    conversation,
);
// 新 session：从 manifest suggested_model 初始化
let initial_model = self.core.manifest.llm.suggested_model.clone();
session_state.set_initial_model(initial_model.clone());
```

如果是从 JSONL 恢复的 session，从 metadata 读取：

```rust
// session_manager.rs create_session()
if let Some(ref conversation) = conversation {
    let meta = conversation.read_metadata();
    if let Some(model) = &meta.model {
        context_builder.set_override_model(model.clone());
    }
}
```

#### 2. model/provider 持久化到 JSONL

与 `workspace_id` 完全相同的模式——写入 `SessionMetadata` 首行：

```rust
// 创建 session 时写入
let meta = SessionMetadata {
    model: Some(initial_model),
    provider: None,
    // ... other fields
};
conversation.update_metadata(&meta);

// model_switch 时更新持久化
fn update_session_model_provider(
    work_dir: &str, session_id: &str,
    model: &str, provider: Option<&str>,
) {
    let path = conversation_path(work_dir, session_id);
    if let Some(mut conversation) = ConversationSession::open(&path) {
        let mut meta = conversation.read_metadata();
        meta.model = Some(model.to_string());
        meta.provider = provider.map(|s| s.to_string());
        conversation.update_metadata(&meta);
    }
}
```

#### 3. model_switch 路由到特定 session

不再 `broadcast()`，不再 `save_agent_model()`：

```rust
// cli.rs process_gateway_recv()
if action == "model_switch" {
    if let Some(model) = params.get("model").and_then(|v| v.as_str()) {
        let provider = params.get("provider").and_then(|v| v.as_str());
        let session_id = params.get("session_id").and_then(|v| v.as_str());

        if let Some(sid) = session_id {
            if let Some(handle) = session_manager.get_session(sid) {
                let _ = handle.send(SessionMessage::ModelSwitch {
                    model: model.to_string(),
                });
                update_session_model_provider(work_dir, sid, model, provider);
            }
        } else {
            tracing::warn!("model_switch missing session_id, ignoring");
        }
    }
    return LoopAction::Continue;
}
```

`SessionTask` 收到 `ModelSwitch` 后更新 `ContextBuilder` 和 `SessionState`：

```rust
// session_task.rs
Some(SessionMessage::ModelSwitch { model }) => {
    context_builder.set_override_model(model.clone());
    agent_loop.session.set_model(Some(model));
}
```

#### 4. `SessionTask::new()` 删除 override_model 参数

不再传递 agent 级别的 `override_model`。`ContextBuilder` 的初始 model 在 `SessionTask::run()` 中从 `SessionState` 读取：

```rust
// session_task.rs
let initial_model = session_state.model()
    .unwrap_or_else(|| self.core.manifest.llm.suggested_model.clone());
context_builder = context_builder.with_override_model(initial_model);
```

#### 5. `QueryConfig` 响应简化

不再从 `load_agent_model()` 读取 model。`QueryConfig` 返回当前 agent 的实际资源信息，model 由各 session 独立管理：

```rust
// cli.rs QueryConfig handler
// 不再 load_agent_model，model 是 per-session 的
let config_snapshot = ConfigSnapshot {
    tools: tool_definitions,
    available_models: session_manager.available_models(),
    // model 字段移除或置空（由前端 per-session 管理）
};
```

#### 6. 启动流程删除 agent_model.json 相关代码

- 删除 `load_agent_model(&config.work_dir)` 调用
- 删除 `saved_provider` 从 `agent_model.json` 回退的逻辑（provider 选择只依赖 manifest）
- 删除 `AgentHelloResult` 后的 `save_agent_model` + `update_model_override` 缓存块
- 删除 `process_gateway_recv` 退出前的 `save_agent_model`

### Gateway 变更

**`chat.rs`** 透传 `session_id`：

```rust
let mut params = serde_json::json!({
    "model": model,
    "message_id": message_id,
});
if let Some(ref p) = provider {
    params["provider"] = serde_json::json!(p);
}
if let Some(sid) = client_msg.session_id {
    params["session_id"] = serde_json::json!(sid);
}
```

## 删除清单（汇总）

| 位置 | 删除项 | 原因 |
|------|--------|------|
| 前端 `AgentState` | `model`, `provider` | 移到 `SessionChatState` |
| 前端 `ChatStore` | `currentModel`, `currentProvider`, `agentModels` | 全局字段不再需要 |
| 前端 `setAvailableModels` | fallback 逻辑 | 不需要 fallback |
| 前端 `setCurrentModel` | 全局字段写入 | 只写 session 级别 |
| 前端 `activateSession` | model/provider 同步 | 由 UI selector 自动派生 |
| 后端 `SessionManagerConfig` | `override_model` | 由 `SessionState` 替代 |
| 后端 `SessionManager` | `update_model_override()` | 不再需要 |
| 后端 `cli.rs` | `save_agent_model`, `load_agent_model`, `AgentModelEntry`, `AGENT_MODEL_FILE` | 不再需要 agent 级别持久化 |
| 后端 `SessionTask::new()` | `override_model` 参数 | 由 `SessionState` 携带 |
| 后端 `model_switch` | `broadcast()` 调用 | 改为单 session 路由 |
| 后端启动流程 | `agent_model.json` 读写、AgentHello model cache | 不再需要 |
| 后端 `QueryConfig` | `load_agent_model` 读取 | model 由各 session 独立管理 |

### `loadAgentModel` 的处理

`loadAgentModel`（`chatStore.ts` L1047）调用 `GET /api/agents/{agentId}/model` 获取 agent 级别的 model，在 ChatPanel agent 启动时调用。改为 per-session 后：

- 当前 active session 的 model 已通过 `model_confirmed` 事件写入 `SessionChatState`
- `loadAgentModel` 不再需要从 HTTP API 获取 agent 级别 model
- **方案**：删除 `loadAgentModel` HTTP 调用，改为直接从 `SessionChatState` 读取 active session 的 model
- ChatPanel L322-324 的初始化逻辑：`loadModels()` 直接调用 `setAvailableModels`，不再先 `await loadAgentModel`

### 实施步骤

**顺序原则**：前端先改（发送 session_id，旧后端忽略 → 无断裂），再改后端。

### Phase 1：前端数据模型 + UI

1. `SessionChatState` 新增 `model: string | null`, `provider: string | null`；`DEFAULT_SESSION_STATE` 初始值 `null`
2. `ChatPanel.tsx` 从 `sessionState?.model` 读 model，删除 `agentState?.model` fallback
3. `setCurrentModel`: 写入 `updateSessionState` + WS 携带 `session_id`（旧后端忽略，无副作用）
4. `model_confirmed` handler: 改为 `updateSessionState` 写入 session 级别（使用已有的 `sid` 变量）
5. 删除 `AgentState.model`, `AgentState.provider`
6. 删除 `ChatStore.currentModel`, `currentProvider`, `agentModels`
7. 删除 `setCurrentModel` 中的 prevModel revert 逻辑（无 WS 时直接 return）
8. `setAvailableModels` 简化为只设置列表
9. `activateSession` 删除 `currentModel`/`currentProvider` sync（UI 自动从 `sessionState` 读取）
10. 删除 `loadAgentModel` 中的 HTTP fetch 调用，ChatPanel 初始化直接 `loadModels()`

### Phase 2：后端 SessionState + 持久化

1. `SessionMetadata` 新增 `model: Option<String>`, `provider: Option<String>`
2. `SessionState` 新增字段和 setter/getter
3. 创建新 session：model 初始值 = manifest `suggested_model`，写入 JSONL metadata
4. 恢复 session：从 metadata 读取 model → 设置到 `context_builder`
5. 提供 `update_session_model_provider()` 持久化函数

### Phase 3：后端 model_switch 改造 + 删除 agent 级别

1. `model_switch`：路由到目标 session（`params["session_id"]`），删除 `broadcast()`
2. 删除 `save_agent_model()` / `load_agent_model()` / `AgentModelEntry` / `AGENT_MODEL_FILE`
3. 删除启动流程中所有 `agent_model.json` 读写代码
4. 删除 `SessionTask::new()` 的 `override_model` 参数
5. 删除 `SessionManagerConfig.override_model`, `update_model_override()`
6. `SessionTask` 收到 `ModelSwitch` 后同时更新 `context_builder` 和 `SessionState`
7. `SessionTask::run()` 从 `SessionState` 读取初始 model 设置到 `context_builder`
8. `QueryConfig` 移除 `load_agent_model` 读取

### Phase 4：Gateway 透传 + 清理

1. Gateway `chat.rs` 透传 `session_id` 到 `params`
2. 验证 model 切换只影响目标 session
3. 验证新建 session 使用 manifest `suggested_model`
4. 验证 session 切换后 model 显示正确恢复
5. 验证 `model_confirmed` 正确写入 session 级别
6. 搜索删除 `agent_model` / `broadcast` / `override_model` / `currentModel` / `agentModels` 等所有残留引用

## 后果

### 正面

- **干净的数据模型**：model/provider 只有一处存储，无冗余、无派生、无兼容层
- **真正的 session 隔离**：切换 model 只影响当前 session，switch session 自动恢复
- **代码量减少**：删除前端 3 个全局字段、后端 2 个函数 + 1 个结构体 + 1 个文件常量
- **与 workspace 模式一致**：JSONL metadata 是 per-session 数据的统一存储层

### 负面

- 旧 JSONL 文件没有 `model/provider` 字段：`Option` 为 `None`，session 回退到 manifest `suggested_model`（可接受的新旧过渡）
- `QueryConfig` 不再返回 agent 级别 model 信息（前端已不再需要）

