# ADR-015: Session Tab UI 改造

> 状态：Proposed
> 日期：2026-05-20

## Context

当前 session 切换是通过 ChatPanel 输入框工具栏中的下拉菜单（SessionPanel）完成的，同一时间只能看到一个 session 的内容。多 session 并行工作时（如一个在等待审批、另一个在 streaming），用户需要反复切换下拉菜单，操作路径长且无法直观感知各 session 状态。

主流 Agent 应用（Cursor、Windsurf、Claude Desktop 等）均采用标签页方式展示多个 session，用户可以同时看到多个打开的 session，点击即切换。

## Decision

将 session 切换从下拉菜单改为**聊天区域顶部标签页**：

```
┌─────────────────────────────────────────────────────────┐
│ [Tab: 代码审查] [Tab: Bug修复] [Tab: 文档] [+] [🕐]     │
├─────────────────────────────────────────────────────────┤
│                                                         │
│              当前 Tab 对应的聊天内容区域                    │
│                                                         │
├─────────────────────────────────────────────────────────┤
│ [输入框]                                    [Send/Stop]  │
│ [ModelMenu] [WorkspaceSelector] [SkillsPanel]           │
└─────────────────────────────────────────────────────────┘
```

- **标签页区域**：聊天区顶部，水平排列已打开的 session tab
- **[+] 按钮**：最右侧，创建新 session 并自动打开 tab
- **[🕐] 按钮**：时钟图标，点击弹出下拉菜单展示全部 session 列表
  - 已打开的 session 显示高亮标记（如小圆点）
  - 点击未打开的 session 会打开新 tab 并切换
  - 点击已打开的 session 会切换到该 tab

## 设计细节

### 1. 新增状态：`openSessionIds`

当前 `AgentState` 只有 `activeSessionId`（一个），需要增加 `openSessionIds: string[]` 来追踪哪些 session 以标签页形式打开。

```typescript
interface AgentState {
  // ...existing...
  activeSessionId: string | null;       // 当前激活的 tab
  openSessionIds: string[];             // 所有已打开的 tab（有序）
}
```

**约束**：
- `openSessionIds` 最大长度 10（防止内存/渲染压力）
- `activeSessionId` 必须 ∈ `openSessionIds`
- Tab 顺序 = `openSessionIds` 数组顺序

### 2. Tab 行为

| 操作 | 行为 |
|------|------|
| 创建 session | 在 `openSessionIds` 末尾追加，设为 active |
| 点击已有 tab | 设为 active，触发 switchSession 流程 |
| 关闭 tab (×) | 从 `openSessionIds` 移除；若关闭的是 active tab，激活相邻 tab |
| 关闭最后一个 tab | 自动创建新 session 并打开 |
| 关闭 streaming tab | 允许关闭（后台继续运行），相邻 tab 激活 |
| 🕐 下拉选择已有 session | 若已打开则切换 tab，否则追加 tab 并切换 |
| 🕐 下拉选择新 session | 同上 |
| Agent 切换 | 恢复该 agent 的 `openSessionIds` 和 `activeSessionId` |

### 3. Tab 样式

参考 VS Code tab 风格：
- Tab 宽度：自适应文本，min 120px，max 200px，文本截断 + tooltip 全名
- Active tab：accent color 底边框 + 背景色区分
- Streaming tab：Loader2 旋转图标替代默认图标
- Waiting approval tab：amber 小圆点指示
- 关闭按钮 (×)：hover 时显示，与 tab 文本右对齐
- Tab 溢出：水平滚动（不折叠）

### 4. LRU 逐出调整

当前 `evictStaleSessions` 最大 5 个 session state。改造后：
- `MAX_CACHED_SESSIONS` 提升到 10（匹配 `openSessionIds` 上限）
- 逐出时保护 `openSessionIds` 中的所有 session（不仅是 activeSessionId）
- 已关闭 tab 但仍在缓存的 session 可被逐出

```typescript
// evictStaleSessions 调整
const toEvict = sorted
  .filter((id) => !agent.openSessionIds.includes(id) && id !== protectSessionId)
  .slice(0, sessionIds.length - MAX_CACHED_SESSIONS);
```

### 5. SessionPanel 下拉菜单改造

SessionPanel 从输入框工具栏移除，功能拆分为：
- **Tab bar 的 [+] 按钮**：创建新 session（原 SessionPanel 底部的 "New Conversation"）
- **Tab bar 的 [🕐] 按钮**：session 列表下拉（原 SessionPanel 的会话列表 + 删除功能）

新的下拉菜单行为：
- 显示全部 session（`sessionStore.sessions`）
- 已打开 tab 的 session 显示 accent color 小圆点
- 点击未打开的 → `openTab(sessionId)` + `switchSession`
- 点击已打开的 → 仅切换 tab
- 保留删除功能（删除时同步关闭 tab）

## 改动清单

### Phase 1：状态层（chatStore + sessionStore）

| 文件 | 改动 |
|------|------|
| `chatStore.ts` | AgentState 增加 `openSessionIds: string[]`；新增 `openTab/closeTab/closeAllTabs` 方法；`activateSession` 同步维护 `openSessionIds`；`evictStaleSessions` 保护 openSessionIds |
| `sessionStore.ts` | `createSession` 完成后调用 `openTab`；`switchSession` 检查 openSessionIds 一致性 |
| `types.ts` | 无改动（SessionStatus 等已完备） |

### Phase 2：SessionTabBar 组件

| 文件 | 改动 |
|------|------|
| 新建 `SessionTabBar.tsx` | Tab 容器组件：渲染 `openSessionIds` 为 tab 列表 + [+] + [🕐] |
| 新建 `SessionTab.tsx` | 单个 tab：标题、状态图标、关闭按钮 |
| 新建 `SessionListDropdown.tsx` | 🕐 下拉菜单：全部 session 列表 + 打开状态标记 + 删除 |

### Phase 3：ChatPanel 集成

| 文件 | 改动 |
|------|------|
| `ChatPanel.tsx` | 在消息区域上方插入 `<SessionTabBar />`；移除底部工具栏的 `<SessionPanel />` |
| `SessionPanel.tsx` | 删除（功能已拆分到 SessionTabBar + SessionListDropdown） |

### Phase 4：边界场景

| 场景 | 处理 |
|------|------|
| WS 推送 session_state_changed 到非 active tab | 已有逻辑（按 session_id 路由），tab 上状态图标更新即可 |
| 删除正在 streaming 的 session | 二次确认，确认后 close tab + 停止 streaming + 删除 |
| Agent 下线重连 | openSessionIds 持久化到 sessionStore，重连后恢复 |
| 窗口 resize / tab 溢出 | 水平滚动，当前 active tab 自动滚入视口 |

## 不改的

- 后端 Runtime/Gateway：无改动，session 管理协议不变
- WS 事件路由：已按 session_id 路由，无需调整
- sessionStore 的 sessions 列表获取逻辑：不变
- ADR-014 SessionStatus 状态机：不变，tab 状态图标直接读取 sessionStatus

## 风险

| 风险 | 缓解 |
|------|------|
| Tab 数量过多导致内存/渲染压力 | `openSessionIds` 上限 10，超出时提示关闭 |
| 关闭 streaming tab 后用户忘记后台任务 | streaming tab 的 × 按钮加 tooltip 提示 |
| Agent 切换时 tab 状态丢失 | openSessionIds 存入 sessionStore 的 agentSessionMap 扩展 |
