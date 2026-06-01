# Workspace 绑定 Session 架构重构

**版本**: v1.0  
**日期**: 2026-05-24  
**状态**: Draft

---

## 概述

将 workspace 的"当前选中"从 Agent 全局（`is_current`）下移到 Session 级别。每个 Session 独立选择工作目录，不同 Session 可以同时工作在相同 Agent 的不同 workspace。

核心原则：
- **workspace 列表**（CRUD）仍是 Agent 级共享资源
- **当前 workspace 选择**是 Session 级私有的
- Agent Home 显式化为 WorkspaceSelector 中的"系统保留项"

---

## 设计决策

| 决策点 | 结论 |
|--------|------|
| Agent Home 表示 | 哨兵值 `"__agent_home__"`（不用 null） |
| 被删 workspace 前台恢复 | 软保留 `pending_workspace_id`，不自动 re-add |
| 列表 CRUD 广播 | 不广播，仅更新当前 session；切换到前台时重新格式化 context |
| `current_dir()` 归属 | 从 `WorkspaceResolver` 移至 `SessionManager` |
| IPC 新增消息 | `SetSessionWorkspace`（Gateway → Runtime，指定 session_id + workspace_id） |
| `agent_workspaces.json` 的 `is_current` | 降级为 `last_active`（仅记录，不驱动 context 格式化） |

---

## 任务分解

### Task 1: Proto / IPC 协议变更

**改动文件**：
- `core/rollball-core/proto/gateway_ipc.proto`

**新增消息**：
```protobuf
// Gateway → Runtime: set the current workspace for a specific session
message SetSessionWorkspace {
    string session_id = 1;
    string workspace_id = 2;  // "__agent_home__" for agent home
}
```

在 `server_message` 的 oneof 中注册：
```protobuf
SetSessionWorkspace set_session_workspace = 35;
```

**改动文件**：
- `core/rollball-core/src/protocol.rs`

在 `GatewayResponse` enum 中新增：
```rust
/// Set the current workspace for a specific session (Gateway → Runtime).
///
/// Unlike WorkspaceConfigUpdate (which pushes the full list),
/// this targets a single session's working directory selection.
SetSessionWorkspace {
    /// Target session ID
    session_id: String,
    /// Workspace ID to activate, or "__agent_home__"
    workspace_id: String,
},
```

**改动文件**：
- `core/rollball-core/src/proto_bridge.rs`

添加 `SetSessionWorkspace` 的序列化/反序列化映射。

---

### Task 2: WorkspaceResolver 瘦身

**改动文件**：
- `core/rollball-runtime/src/tools/workspace_resolver.rs`

**删除**：
- `WorkspaceResolver.current_dir_index` 字段
- `WorkspaceResolver::current_dir()` 方法
- 测试 `test_resolver_no_config_file`, `test_resolver_with_current_workspace`, `test_resolver_no_current_workspace`（用 `SessionManager` 层测试替代）

**保留**：
- `agent_home()` — 不变
- `search_dirs()` — 不变（工具搜索仍遍历所有 workspace + agent_home）
- `allowed_dirs()` — 不变（路径验证仍用全列表）
- `reload()`, `new()` — 保留但不再读取 `is_current`

**字段变更** `WorkspaceDirFull`：
```rust
// Before:
pub is_current: bool,

// After: rename to last_active, still stored but not used for context formatting
// Session-level selection takes precedence.
#[serde(default)]
pub last_active: bool,
```

**`agent_workspaces.json` 解析**：
- `load_workspace_dirs()` 不再返回 `current_dir_index`
- `WorkspaceResolver` 不再有 `current_dir_index` 字段

---

### Task 3: SessionManager 获取 per-session workspace

**改动文件**：
- `core/rollball-runtime/src/agent/session/session_manager.rs`

**新增字段** `SessionManager`：
```rust
pub struct SessionManager {
    // ... existing fields ...

    /// Per-session workspace selection.
    /// Maps session_id → workspace_id (or "__agent_home__").
    session_workspaces: HashMap<String, String>,

    /// Per-session pending workspace reference.
    /// When a session's last workspace was deleted from the list,
    /// the session_id → ws_id mapping is moved here so it can be
    /// reconciled if the workspace is re-added. When the session
    /// switches to foreground and its ws_id is not in the list,
    /// fallback to agent home while keeping this reference.
    pending_workspaces: HashMap<String, String>,
}
```

**新增方法**：
```rust
/// Set the current workspace for a specific session.
pub fn set_session_workspace(&mut self, session_id: &str, workspace_id: &str) {
    self.session_workspaces.insert(session_id.to_string(), workspace_id.to_string());
    // Remove from pending if the workspace is now active
    self.pending_workspaces.remove(session_id);
}

/// Get the current working directory path for a session.
/// Returns (path, is_agent_home).
pub fn current_dir_for(&self, session_id: &str) -> (&str, bool) {
    match self.session_workspaces.get(session_id) {
        Some(id) if id == "__agent_home__" => (&self.agent_home, true),
        Some(id) => {
            // Look up path from allowed_dirs
            // Falls back to agent_home if not found (deleted workspace)
            ...
        }
        None => (&self.agent_home, true), // new session defaults to agent home
    }
}

/// Format and send workspace context to a specific session.
pub fn update_session_workspace_context(
    &mut self,
    session_id: &str,
    resolver: &WorkspaceResolver,
) {
    let (current_path, _) = self.current_dir_for(session_id);
    let context_text = format_workspace_context_for_session(
        resolver,
        current_path,
        session_id,
        self.session_workspaces.get(session_id),
    );
    // Send UpdateWorkspaceContext to the specific session only
    if let Some(handle) = self.sessions.get(session_id) {
        let _ = handle.send(SessionMessage::UpdateWorkspaceContext { context_text });
    }
}
```

**修改 `create_session`**：
- 新 session 的 `session_workspaces` 默认值为 `"__agent_home__"`
- 不再 replay 全局 `workspace_context`；改为首次 build 时根据 session 的 workspace 格式化

**修改 `set_workspace_context`**：
- 改为接受 `session_id` 参数：
```rust
pub fn set_workspace_context_for(
    &mut self,
    session_id: &str,
    context_text: String,
) {
    if let Some(handle) = self.sessions.get(session_id) {
        let _ = handle.send(SessionMessage::UpdateWorkspaceContext { context_text });
    }
}
```

---

### Task 4: Context Formatting 改造

**改动文件**：
- `core/rollball-runtime/src/tools/workspace_resolver.rs`

**新增函数**：
```rust
/// Format workspace context for a specific session.
///
/// Unlike the old `format_workspace_context_from_json`, this takes
/// the pre-loaded `WorkspaceResolver` and the session's current
/// workspace selection rather than reading `is_current` from JSON.
pub fn format_workspace_context_for_session(
    resolver: &WorkspaceResolver,
    current_path: &str,
    _session_id: &str,
    current_ws_id: Option<&String>,
) -> String {
    // Build Markdown:
    // 1. "Current Working Directory: {current_path} (alias, access)"
    //    + "(agent home)" suffix when current_ws_id == "__agent_home__"
    // 2. "Agent Home Directory: {agent_home}"
    // 3. Available Workspaces table (no Active column or no * marker)
    //    — just list all workspaces from resolver.allowed_dirs()
}
```

**重构 `compute_context_workspaces`**：
- 移除 `is_current` 优先逻辑
- 按 `select_count` + `recency` 排序，取 top 3
- 当前选中的 workspace 始终排第一（无论分数）

---

### Task 5: Runtime CLI 处理 WorkspaceConfigUpdate

**改动文件**：
- `core/rollball-runtime/src/cli.rs`

**修改 `WorkspaceConfigUpdate` 处理**：
```rust
GatewayResponse::WorkspaceConfigUpdate { config_json } => {
    // 1. Write config to agent_workspaces.json
    write_workspace_config(work_dir, &config_json)?;

    // 2. Reload WorkspaceResolver (hot-reload path whitelist)
    {
        let mut w = resolver.write().unwrap();
        *w = WorkspaceResolver::reload(work_dir);
    }

    // 3. Refresh context for CURRENT session only (not broadcast)
    let current_sid = session_manager.current_session_id();
    session_manager.update_session_workspace_context(
        &current_sid,
        &resolver.read().unwrap(),
    );

    // 4. For all other sessions: check if their selected workspace
    //    still exists. If deleted → move to pending, fallback to agent home.
    let resolver_guard = resolver.read().unwrap();
    session_manager.reconcile_deleted_workspaces(&resolver_guard);

    return LoopAction::Continue;
}
```

**新增 `WorkspaceConfigUpdate` 处理**：
```rust
GatewayResponse::SetSessionWorkspace { session_id, workspace_id } => {
    // Validate workspace exists or is "__agent_home__"
    let is_valid = workspace_id == "__agent_home__"
        || resolver.read().unwrap()
            .allowed_dirs()
            .iter()
            .any(|d| d.id == workspace_id); // NOTE: WorkspaceDir needs id field now

    if !is_valid {
        // Workspace not in list → set as pending, fallback
        session_manager.pending_workspaces.insert(session_id.clone(), workspace_id.clone());
        session_manager.set_session_workspace(&session_id, "__agent_home__");
    } else {
        session_manager.set_session_workspace(&session_id, &workspace_id);
    }

    session_manager.update_session_workspace_context(
        &session_id,
        &resolver.read().unwrap(),
    );

    return LoopAction::Continue;
}
```

---

### Task 6: WorkspaceResolver 的 WorkspaceDir 结构补全

**改动文件**：
- `core/rollball-runtime/src/tools/workspace_resolver.rs`

当前 `WorkspaceDir`（内部用）缺少 `id` 字段，但 `WorkspaceDirFull`（序列化用）有。需统一：

```rust
pub struct WorkspaceDir {
    pub id: String,        // NEW
    pub path: String,
    pub access: WorkspaceAccess,
}
```

`load_workspace_dirs()` 解析时填入 `id`。

---

### Task 7: Gateway HTTP API 变更

**改动文件**：
- `core/rollball-gateway/src/http/workspaces.rs`

**废弃 `PUT /api/agents/{agent_id}/workspaces/current`**：
- 该 API 现在不再修改全局 `is_current`，改为：
  - 不再序列化/修改 `is_current` 字段
  - 只更新 `select_count` 和 `last_selected_at`（用于排序权重）
  - 返回更新后的列表，但不再包含 `is_current: true` 条目

**新增（或复用现有路由）**：
```
PUT /api/agents/{agent_id}/sessions/{session_id}/workspace/current
  Body: { "workspace_id": "ws-abc123" | "__agent_home__" }
```

此路由 Gateway 不直接处理 disk/cache 变更，只做两件事：
1. 将 `SetSessionWorkspace` 通过 IPC 推送给对应 agent 的 Runtime
2. 不更新 `workspace_config_json` 缓存（列表没变）

**但实际上**，考虑到前端调用链复杂度，也可以简单方案：让 Gateway 在 `SetSessionWorkspace` 时同时更新 `select_count`/`last_selected_at` 并推送完整的 `WorkspaceConfigUpdate`（列表本身不变但统计变化），然后用单独的 IPC 消息设置 session workspace。但这会导致 Runtime 收到两次更新。

**权衡后方案**：复用现有路由，但新增 query 参数：

```
PUT /api/agents/{agent_id}/workspaces/current?session_id={session_id}
  Body: { "workspace_id": "ws-abc123" | "__agent_home__" }
```

Gateway 处理：
1. 找到 agent 的 Runtime IPC 通道
2. 发送 `SetSessionWorkspace { session_id, workspace_id }`
3. 更新 `workspace_config_json` 缓存（select_count, last_selected_at 变化）
4. 发送 `WorkspaceConfigUpdate` 更新列表统计

这样前端改动最小。

---

### Task 8: 新增 `is_path_allowed()` on WorkspaceResolver

**改动文件**：
- `core/rollball-runtime/src/tools/workspace_resolver.rs`

`SessionManager::current_dir_for()` 需要验证 workspace_id 对应的路径是否仍在列表中。目前 `WorkspaceResolver` 需要暴露 workspace id → path 的查找能力：

```rust
impl WorkspaceResolver {
    /// Find a workspace directory by its ID. Returns None if not found.
    pub fn find_by_id(&self, id: &str) -> Option<&WorkspaceDir> {
        self.allowed_dirs.iter().find(|d| d.id == id)
    }
}
```

---

### Task 9: 前端 workspaceStore 改造

**改动文件**：
- `apps/rollball-desktop/src/stores/workspaceStore.ts`

**核心变更**：`currentWorkspaceId` 从全局 string 变为 per-session 映射：

```typescript
interface WorkspaceState {
  workspaces: WorkspaceDir[];
  /** Per-session current workspace selection. "__agent_home__" = agent home. */
  sessionWorkspaceMap: Record<string, string>;  // NEW: sessionId → workspaceId
  loading: boolean;

  fetchWorkspaces: (agentId: string) => Promise<void>;

  /** Set current workspace for a specific session. */
  setSessionWorkspace: (
    agentId: string,
    sessionId: string,
    workspaceId: string
  ) => Promise<void>;

  /** Get current workspace ID for a session. Defaults to "__agent_home__". */
  getSessionWorkspaceId: (sessionId: string) => string;

  reset: () => void;
}
```

**`fetchWorkspaces`**：不再读 `is_current` 推导 `currentWorkspaceId`。

**`setSessionWorkspace`**：
```typescript
setSessionWorkspace: async (agentId, sessionId, workspaceId) => {
  const baseUrl = getGatewayUrl();
  const resp = await fetch(
    `${baseUrl}/api/agents/${agentId}/workspaces/current?session_id=${sessionId}`,
    {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ workspace_id: workspaceId }),
    }
  );
  if (resp.ok) {
    const data = await resp.json() as { workspaces: WorkspaceDir[] };
    set({
      workspaces: data.workspaces || [],
      sessionWorkspaceMap: {
        ...get().sessionWorkspaceMap,
        [sessionId]: workspaceId,
      },
    });
  }
}
```

**Agent 切换时 `reset`**：
```typescript
reset: () => {
  set({ workspaces: [], sessionWorkspaceMap: {}, loading: false });
}
```

---

### Task 10: WorkspaceSelector UI — Agent Home 显式化

**改动文件**：
- `apps/rollball-desktop/src/components/workspace/WorkspaceSelector.tsx`

**新 props / 数据获取**：
- 从 `useSessionStore` 获取 `activeSessionId`
- 用 `sessionWorkspaceMap[sessionId]` 代替 `currentWorkspaceId`

**下拉列表渲染**：
```
┌─────────────────────────────────┐
│  🏠 Agent Home        ✓        │  ← 常驻，不可删除，不可改权限
│  C:\Users\...\.agent\install\   │
│─────────────────────────────────│  ← 分隔线
│  📁 frontend           (RW)     │
│  📁 backend            (RO)     │
│─────────────────────────────────│
│  + Add workspace...             │
└─────────────────────────────────┘
```

**Agent Home 行**：
- 图标 `HomeIcon`（可复用现有 `FolderIcon`，改颜色或换图标）
- 标签 "Agent Home"
- 路径显示 agent 安装目录（从 selected agent 的 `work_dir` 获取）
- 选中时显示 ✓ checkmark
- 点击调用 `setSessionWorkspace(agentId, sessionId, "__agent_home__")`
- 不渲染删除按钮、权限切换按钮

**用户 workspace 行**：
- 不再显示 `is_current` 的 `*` 标记（无意义，因为 current 是 session 级）
- 保留权限切换、删除按钮

**按钮显示文字**：
- 选中 Agent Home → 显示 "Agent Home"
- 选中某 workspace → 显示 alias 或缩写 path（同现在）

**`handleDelete`**：
- 如果被删 workspace 是当前 session 选中的 → 前端主动 fallback 到 Agent Home：
```typescript
if (get().sessionWorkspaceMap[sessionId] === id) {
  await setSessionWorkspace(agentId, sessionId, "__agent_home__");
}
await deleteWorkspace(id);
void fetchWorkspaces(agentId);
```

---

### Task 11: Session 切换时的 workspace 同步

**改动文件**：
- `apps/rollball-desktop/src/components/chat/ChatPanel.tsx`

ChatPanel 中 `WorkspaceSelector` 需要响应 session 切换：

```typescript
useEffect(() => {
  if (!selectedAgentId || !activeSessionId) return;
  // Ensure workspace list is loaded for this agent
  const state = useWorkspaceStore.getState();
  if (state.workspaces.length === 0) {
    void state.fetchWorkspaces(selectedAgentId);
  }
  // Note: session's current workspace is already in sessionWorkspaceMap,
  // managed by WorkspaceSelector's handleSelect
}, [selectedAgentId, activeSessionId]);
```

实际上 WorkspaceSelector 本身就从 `sessionWorkspaceMap[sessionId]` 读取，session 切换时 `activeSessionId` 变化 → selector 自动反映新 session 的 current workspace。不需要额外逻辑。

**但需要验证**：如果切换到的新 session 的 workspace 已从列表删除，Runtime 端已 fallback 到 agent home，前端如何感知？
- 答：前端通过 `fetchWorkspaces` 获取列表，发现 `sessionWorkspaceMap[sessionId]` 对应的 workspace 不在列表 → 自动 fallback 到 `"__agent_home__"` 并显示 "Workspace (removed)" 提示。

---

### Task 12: Pending Workspace 前端 UI

**改动文件**：
- `apps/rollball-desktop/src/components/workspace/WorkspaceSelector.tsx`

当 `sessionWorkspaceMap[sessionId]` 对应的 workspace 不在当前列表中时：

1. **按钮显示**：`⚠ Workspace (removed)` 
2. **下拉列表**：在 Agent Home 和用户 workspace 之间插入灰色提示行：
```
⚠ The workspace selected for this session has been removed.
  Re-add the directory to restore it, or select another workspace.
```

用户选择其他 workspace 后，`pending_workspace_id` 被覆盖。

---

### Task 13: Agent 启动时的 workspace 初始化

**改动文件**：
- `core/rollball-runtime/src/cli.rs`

AgentHello 响应后，不再调用 `session_manager.set_workspace_context(fallback)` 广播。改为：

1. 初始 session 创建时，`session_workspaces` 默认为 `"__agent_home__"`
2. `agent_workspaces.json` 存在时：读取列表，但 `is_current` 仅用于填充 `last_active`（向前兼容），不影响 context
3. 初始 session 首次 build context 时，由 `SessionManager` 按 session 的 workspace 格式化

**兼容旧数据**：
- 如果 `agent_workspaces.json` 中有 `is_current: true` 的条目 → 初始 session 选中该 workspace（不是 fallback 到 agent home）
- 写入新的 `agent_workspaces.json` 时，`is_current` 改为 `last_active`（只保留最后一个 is_current 为 true 的条目）

---

### Task 14: 测试更新

**受影响测试**：

| 测试文件 | 变更 |
|----------|------|
| `core/rollball-runtime/src/tools/workspace_resolver.rs` tests | 删除 `current_dir()` 相关测试；新增 `find_by_id()` 测试 |
| `core/rollball-gateway/tests/` workspace tests | 新增 `SetSessionWorkspace` IPC 端到端测试；更新 `set_current_workspace` 不再依赖 `is_current` |
| `core/rollball-runtime/tests/` session tests | 新增 per-session workspace 隔离测试 |
| `core/rollball-core/tests/` protocol tests | 新增 `SetSessionWorkspace` 序列化/反序列化测试 |

---

### Task 15: 删除 workspace 时的级联清理

**场景**：用户删除一个 workspace 目录，该 workspace 被多个 session 引用。

**处理**：
1. Gateway 发送 `WorkspaceConfigUpdate`（移除了该条目）
2. Runtime 处理：
   ```rust
   // Scan all sessions, find any that reference the deleted workspace
   for (sid, ws_id) in &session_manager.session_workspaces {
       if !resolver.find_by_id(ws_id).is_some() && ws_id != "__agent_home__" {
           session_manager.pending_workspaces.insert(sid.clone(), ws_id.clone());
           session_manager.session_workspaces.insert(sid.clone(), "__agent_home__".to_string());
       }
   }
   ```
3. 仅对 **当前前台 session** 和**被删除 workspace 的 session** 更新 context；其他 session 延迟到前台切换时再更新

---

### Task 16: Pending workspace 恢复时的自动激活

**场景**：用户删除 workspace 后，又重新添加了相同路径的 workspace（id 不同）。

**处理**：
- `pending_workspaces` 存的是旧 `workspace_id`
- 新添加的 workspace 有新的 id
- 无法自动匹配（id 不同）
- **结论**：不自动恢复；`pending_workspace_id` 仅用于 UI 提示 "The workspace was removed"，用户在 WorkspaceSelector 手动选择新 workspace 后覆盖

**但可以增强**：在 `add_workspace` Gateway handler 中，如果新添加的 path 匹配某个 `pending_workspaces` 中的旧 path → 自动映射。但这需要 Gateway 访问 Runtime 的 `pending_workspaces`，跨进程复杂。**先不实现，作为后续优化。**

---

## 变更文件总览

| 文件 | 改动类型 | 行数估计 |
|------|----------|----------|
| `core/rollball-core/proto/gateway_ipc.proto` | 新增消息 | +5 |
| `core/rollball-core/src/protocol.rs` | 新增 enum 变体 | +10 |
| `core/rollball-core/src/proto_bridge.rs` | 序列化映射 | +15 |
| `core/rollball-runtime/src/tools/workspace_resolver.rs` | 删除 current_dir, 新增 find_by_id, 重构 context 格式化 | ~120 改动 |
| `core/rollball-runtime/src/agent/session/session_manager.rs` | 新增字段+方法, 重构 set_workspace_context | ~100 新增 |
| `core/rollball-runtime/src/cli.rs` | 重构 WorkspaceConfigUpdate 处理, 新增 SetSessionWorkspace 处理 | ~150 改动 |
| `core/rollball-gateway/src/http/workspaces.rs` | 修改 set_current_workspace, 新增 session query param | ~50 改动 |
| `apps/rollball-desktop/src/stores/workspaceStore.ts` | 重构为 per-session map | ~40 改动 |
| `apps/rollball-desktop/src/components/workspace/WorkspaceSelector.tsx` | Agent Home 显式化, 删除 workspace fallback | ~100 改动 |
| 测试文件 | 新增/更新 | ~200 新增 |

---

## 实施顺序

1. **Task 1** (Proto/IPC) → 协议先行，后续任务可并行
2. **Task 2+6** (WorkspaceResolver 瘦身+结构补全) → 基础层变更
3. **Task 3+8** (SessionManager + find_by_id) → 核心逻辑
4. **Task 4** (Context Formatting) → 格式化适配
5. **Task 5** (CLI 处理) → 接入新协议
6. **Task 7** (Gateway HTTP) → API 适配
7. **Task 9** (workspaceStore) → 前端状态
8. **Task 10+11+12** (WorkspaceSelector UI) → 前端 UI
9. **Task 13** (启动初始化) → 兼容旧数据
10. **Task 14** (测试) → 最后验证
11. **Task 15+16** (级联清理+恢复) → 边界情况
