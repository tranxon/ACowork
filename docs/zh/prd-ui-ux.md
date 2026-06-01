# Rollball Desktop App — UI/UX 产品需求文档

> 版本：v1.0 | 日期：2026-04-27
> 关联设计文档：`docs/design/14-desktop-app.md`
> 关联实施计划：`docs/plan/plan-p5.md`（S1 用户模式）

---

## 1. 文档目的

本文档定义 Rollball Desktop App 的 **用户模式** 全部页面交互规格，作为 Phase 5 S1 前端开发的唯一实现依据。开发者模式 UI 将在 S2 阶段补充。

`docs/design/14-desktop-app.md` 定义了架构、布局结构和技术选型，本文档在其基础上补充：
- 每个页面的详细交互流程、状态转换、边界条件
- 组件规格（数据源、事件、状态）
- API 契约映射（前端操作 → Gateway HTTP API）
- 错误处理 UX
- 视觉规范（设计 token、状态样式、暗色模式）

---

## 2. 设计系统

### 2.1 设计 Token

基于 shadcn/ui + Tailwind CSS，定义项目级设计变量：

```css
/* 颜色 */
--color-primary:       hsl(222.2 47.4% 11.2%);    /* 深蓝黑 */
--color-primary-foreground: hsl(210 40% 96.1%);    /* 近白 */
--color-accent:        hsl(210 40% 96.1%);         /* 浅灰蓝 */
--color-accent-foreground: hsl(222.2 47.4% 11.2%); /* 深蓝黑 */
--color-destructive:   hsl(0 84.2% 60.2%);         /* 红色 */
--color-muted:         hsl(210 40% 96.1%);         /* 浅灰 */
--color-muted-foreground: hsl(215.4 16.3% 46.9%);  /* 中灰 */

/* 语义色 */
--color-agent-running:  hsl(142 76% 36%);          /* 绿色 */
--color-agent-stopped:  hsl(215.4 16.3% 46.9%);    /* 灰色 */
--color-agent-error:    hsl(0 84.2% 60.2%);         /* 红色 */
--color-agent-starting: hsl(45 93% 47%);            /* 橙色（启动中）*/
--color-gateway-ok:     hsl(142 76% 36%);           /* 绿色 */
--color-gateway-off:    hsl(215.4 16.3% 46.9%);     /* 灰色 */
--color-gateway-error:  hsl(0 84.2% 60.2%);         /* 红色 */

/* 间距 */
--spacing-nav:         48px;       /* 导航栏宽度 */
--spacing-agent-list:  240px;      /* Agent 列表宽度 */
--spacing-results:     320px;      /* 执行结果区宽度 */
--spacing-chat-min:    360px;      /* 聊天面板最小宽度 */

/* 字号 */
--text-xs:    0.75rem;   /* 12px — 辅助信息 */
--text-sm:    0.875rem;  /* 14px — 列表项、标签 */
--text-base:  1rem;      /* 16px — 正文、输入框 */
--text-lg:    1.125rem;  /* 18px — 小标题 */
--text-xl:    1.25rem;   /* 20px — 页面标题 */

/* 圆角 */
--radius-sm:  4px;
--radius-md:  6px;
--radius-lg:  8px;

/* 动画 */
--duration-fast:   150ms;    /* hover、focus */
--duration-normal: 250ms;    /* 面板展开/折叠 */
--duration-slow:   400ms;    /* 页面切换 */

/* 暗色模式 */
/* 通过 Tailwind `dark:` 前缀实现，颜色反转规则： */
/*   bg-white → bg-zinc-900, text-zinc-900 → text-zinc-100 */
/*   border-zinc-200 → border-zinc-800 */
```

### 2.2 窗口规格

| 属性 | 值 |
|------|-----|
| 最小尺寸 | 1024 × 600 |
| 默认尺寸 | 1200 × 800 |
| 关闭行为 | 隐藏到托盘（不退出） |
| 单实例 | 是（`tauri-plugin-single-instance`） |

### 2.3 响应式断点

| 断点 | 窗口宽度 | 布局调整 |
|------|---------|---------|
| 宽屏 | ≥1280px | 四栏全展开 |
| 标准 | 1024~1279px | Agent 列表收窄到 200px，结果区可折叠 |
| 窄屏 | <1024px | 不支持（最小尺寸限制） |

---

## 3. 全局组件

### 3.1 顶部标题栏

```
┌──────────────────────────────────────────────────────────────────┐
│  ◉ Rollball          [Agent Name]          [Dev Mode ○]  [— □ ✕]│
└──────────────────────────────────────────────────────────────────┘
```

| 元素 | 位置 | 说明 |
|------|------|------|
| ◉ Gateway 状态指示器 | 左 | 绿色=已连接，灰色=未连接，红色=错误。点击重连 |
| "Rollball" 品牌 | 左 | 固定文本 |
| 当前 Agent 名称 | 中 | 选择 Agent 后显示，无 Agent 时显示 "Select an agent" |
| Developer Mode toggle | 右 | Phase 5 S2 启用，S1 阶段灰色不可点击 |
| 窗口控制按钮 | 右 | Tauri 原生装饰，最小化/最大化/关闭 |

**Gateway 状态指示器交互**：

| 状态 | 图标 | Tooltip | 点击行为 |
|------|------|---------|---------|
| Connected | 🟢 绿色圆点 | "Gateway Connected" | 无操作 |
| Disconnected | ⚪ 灰色圆点 | "Gateway Disconnected — Click to reconnect" | 尝试连接 Gateway，显示加载 spinner 3s |
| Error | 🔴 红色圆点 | "Gateway Error — {error message}" | 尝试连接 Gateway |

**Gateway 断连横幅**：当 Gateway 连续 3 次健康检查失败，在主内容区顶部显示横幅：

```
┌─────────────────────────────────────────────────────────────┐
│ ⚠ Gateway 未连接。请启动 Gateway 或检查连接配置。  [设置] [重试] │
└─────────────────────────────────────────────────────────────┘
```

- [设置]：跳转 Settings → Gateway
- [重试]：立即执行健康检查

### 3.2 导航栏（最左侧，48px）

```
┌────┐
│ 💬 │  ← Chat（默认视图）
├────┤
│ 🤖 │  ← Models（Provider 管理）
├────┤
│ 📋 │  ← Skills（S3 启用，S1 灰色）
├────┤
│ ⚙️ │  ← Settings
└────┘
```

| 规则 | 说明 |
|------|------|
| 选中态 | 背景 `--color-accent`，图标颜色加深 |
| 未选中态 | 透明背景，图标颜色 `--color-muted-foreground` |
| 禁用态 | 图标 50% 透明度，Tooltip 显示 "Available in Developer Mode" |
| Hover | 背景浅色高亮 |
| Tooltip | 鼠标悬停 300ms 后显示 |

### 3.3 系统托盘

| 状态 | 图标 | Tooltip | 右键菜单 |
|------|------|---------|---------|
| Gateway 已连接 + 无 Agent 运行 | 正常图标 | "Rollball — Connected" | Show Dashboard / Agent Chat / ── / Status: Connected / ── / Quit |
| Gateway 已连接 + Agent 运行 | 正常图标+蓝色脉冲 | "Rollball — {N} Agents Running" | 同上 + Status: N Agents Running |
| Agent 执行中 | 正常图标+绿色脉冲 | "Rollball — Working" | 同上 |
| Gateway 未连接 | 灰色图标 | "Rollball — Disconnected" | Show Dashboard / Agent Chat / ── / Start Gateway / ── / Quit |
| 错误 | 红色图标 | "Rollball — Error" | Show Dashboard / ── / Quit |

---

## 4. Chat 视图（默认视图）

### 4.1 视图结构

```
┌────┬──────────────┬────────────────────────┬────────────────────┐
│    │ Agent List   │     Chat Panel         │  Results Panel     │
│ N  │              │                        │                    │
│ A  │  ┌────────┐  │  ┌──────────────────┐  │  (见 §4.3)        │
│ V  │  │☀ AgentA│  │  │ System:          │  │                    │
│    │  │  AgentB│  │  │ User: Hello      │  │                    │
│    │  │  AgentC│  │  │ Assistant: Hi!   │  │                    │
│    │  │        │  │  │ Tool: http_req   │  │                    │
│    │  │ [+ 📦] │  │  │  └▶ Details     │  │                    │
│    │  └────────┘  │  │ Assistant: Here  │  │                    │
│    │              │  └──────────────────┘  │                    │
│    │              │  ┌──────────────────┐  │                    │
│    │              │  │ [Type message...] │  │                    │
│    │              │  │            [Send] │  │                    │
│    │              │  └──────────────────┘  │                    │
└────┴──────────────┴────────────────────────┴────────────────────┘
```

### 4.2 Agent 列表（第二栏，240px）

#### 数据源

| 数据 | API | 刷新时机 |
|------|-----|---------|
| Agent 列表 | `GET /api/agents` | 初始加载 + 安装/卸载后 + 每 30s 轮询 |
| Agent 运行状态 | 同上（`status` 字段） | 同上 + 启停操作后 |

**`GET /api/agents` 响应格式**：

```json
[
  {
    "agent_id": "com.example.weather",
    "name": "Weather Agent",
    "description": "查询天气信息",
    "version": "1.0.0",
    "status": "running",
    "dev": false,
    "icon": null
  }
]
```

#### Agent 条目

```
┌─────────────────────────────────────┐
│ [🤖]  Weather Agent          🟢    │
│       com.example.weather           │
└─────────────────────────────────────┘
```

| 元素 | 说明 |
|------|------|
| 图标 | Agent manifest `icon` 字段，无则显示默认 🤖 |
| 名称 | `name` 字段，单行截断 |
| Agent ID | `agent_id` 字段，`text-xs` + `text-muted-foreground`，单行截断 |
| 状态指示器 | 🟢 running / ⚪ stopped / 🟠 starting / 🔴 error |
| dev 标签 | `dev: true` 时显示 "DEV" 小标签（橙色背景） |

#### Agent 条目交互

| 操作 | 触发 | 行为 |
|------|------|------|
| 点击 | 左键 | 选中该 Agent，Chat Panel 加载该 Agent 对话 |
| 右键 | 右键 | 弹出上下文菜单 |

**右键上下文菜单**：

| 菜单项 | 条件 | 行为 |
|--------|------|------|
| ▶ Start | `status === "stopped"` | `POST /api/agents/{id}/start` |
| ⏹ Stop | `status === "running"` | `POST /api/agents/{id}/stop`，需确认 |
| 📋 Details | 始终 | 打开 Agent 详情弹窗 |
| 🔗 Clone | S4 启用，S1 灰色 | 打开克隆对话框 |
| 🗑 Uninstall | 始终 | 确认后 `DELETE /api/agents/{id}` |

**Stop / Uninstall 确认对话框**：

```
┌─ Stop Agent ────────────────────────┐
│                                      │
│  确定要停止 Weather Agent 吗？       │
│  当前对话状态将保留。                │
│                                      │
│            [取消]  [停止]            │
└──────────────────────────────────────┘
```

- 取消按钮：默认焦点
- 危险操作按钮：`destructive` 样式（红色）
- Esc 键：关闭对话框（等同取消）

#### 底部操作区

```
┌─────────────────────────────────────┐
│  [+ 安装 Agent]   [📦 从文件安装]    │
└─────────────────────────────────────┘
```

| 按钮 | 行为 |
|------|------|
| + 安装 Agent | S1 阶段打开文件选择对话框（`.agent` 文件），调用 `POST /api/agents/install` |
| 📦 从文件安装 | 同上（两个入口，行为一致） |

**安装进度**：

1. 文件选择后，按钮变为 spinner + "Installing..."
2. API 成功 → Agent 出现在列表中，自动选中
3. API 失败 → Toast 错误通知（见 §8）

### 4.3 Chat Panel（第三栏，弹性宽度）

#### 4.3.1 空状态

未选择 Agent 或 Agent 无对话时：

```
┌──────────────────────────────────────────┐
│                                          │
│          🤖                              │
│                                          │
│    选择一个 Agent 开始对话                │
│    或安装新的 Agent                       │
│                                          │
│          [浏览 Agent]                    │
│                                          │
└──────────────────────────────────────────┘
```

#### 4.3.2 Agent 未运行状态

选择了 Agent 但 Agent 未运行时：

```
┌──────────────────────────────────────────┐
│                                          │
│          ⏸                               │
│                                          │
│    Weather Agent 已停止                   │
│                                          │
│          [▶ 启动 Agent]                  │
│                                          │
└──────────────────────────────────────────┘
```

- [▶ 启动 Agent]：调用 `POST /api/agents/{id}/start`，按钮变为 spinner + "Starting..."
- 启动成功后自动切换到对话视图

#### 4.3.3 消息流

**消息类型与样式**：

| 消息类型 | 对齐 | 背景色 | 头部 | 内容 |
|---------|------|--------|------|------|
| `user` | 右对齐 | `--color-accent` | 无 | 用户输入文本 |
| `assistant` | 左对齐 | 白色/暗色模式 `zinc-800` | 无 | Markdown 渲染 |
| `system` | 居中 | `--color-muted` | "System" | 小字、灰色 |
| `tool_call` | 左对齐 | 浅蓝 `blue-50` / 暗色 `blue-950` | "🔧 {tool_name}" | 参数 JSON，可折叠 |
| `tool_result` | 左对齐 | 浅绿 `green-50` / 暗色 `green-950` | "🔧 {tool_name} → Result" | 结果 JSON，默认折叠 |

**消息气泡规格**：

```
User:                              Assistant:
                    ┌──────────┐   ┌──────────────────────┐
                    │  Hello   │   │ Hi! How can I help?  │
                    └──────────┘   └──────────────────────┘
                     max-width 70%         max-width 85%
```

- 消息间距：8px
- 气泡内边距：12px 16px
- 气泡圆角：`--radius-lg`（上方两角 4px，下方两角 12px）

**Tool Call 折叠**：

```
┌─ 🔧 http_request ─────────────── ▶ 展开 ─┐
│  GET https://api.weather.com/v1?city=BJ   │
│  耗时: 1.2s                               │
└────────────────────────────────────────────┘

展开后：
┌─ 🔧 http_request ─────────────── ▼ 收起 ─┐
│  Request:                                 │
│    method: GET                            │
│    url: https://api.weather.com/v1?city=BJ│
│  Response:                                │
│    { "temp": 25, "condition": "晴" }      │
│  耗时: 1.2s  状态: ✅ 成功                 │
└────────────────────────────────────────────┘
```

- 默认折叠，仅显示工具名 + 首行参数 + 耗时 + 状态
- 展开后显示完整参数和结果
- 工具失败时状态显示 ❌ + 错误信息

#### 4.3.4 流式输出

Assistant 消息的流式渲染：

1. 收到 `chunk` 事件 → 逐字追加到当前气泡
2. 打字光标（▌）在最后一个字符后闪烁
3. 收到 `done` 事件 → 移除光标，显示最终消息
4. 流式中收到 `tool_call` → 先完成当前文本，再插入工具调用气泡

**WebSocket 事件映射**：

| 事件 | 行为 |
|------|------|
| `{ "type": "chunk", "delta": "今", "message_id": "msg-001" }` | 追加字符到消息气泡 |
| `{ "type": "tool_call", "name": "http_request", "params": {...} }` | 插入折叠的工具调用气泡 |
| `{ "type": "tool_result", "name": "http_request", "result": {...} }` | 填充工具调用气泡的结果 |
| `{ "type": "done", "message_id": "msg-001", "usage": {...} }` | 结束流式，更新 Token 统计 |

#### 4.3.5 输入区

```
┌──────────────────────────────────────────────────────┐
│  [Type a message...                    ]  [Send ➤]  │
└──────────────────────────────────────────────────────┘
```

| 规则 | 说明 |
|------|------|
| 多行输入 | 按 Enter 发送，Shift+Enter 换行 |
| 最大长度 | 32KB（与 API 限制一致），接近上限时显示字符计数 |
| 发送按钮 | 有内容时可用（蓝色），无内容时禁用（灰色） |
| 发送中 | 输入框禁用 + 发送按钮变为 spinner，直到收到 `done` 事件 |
| Agent 未运行 | 输入框禁用，Placeholder："Agent is not running" |
| Gateway 断连 | 输入框禁用，Placeholder："Gateway not connected" |

**发送消息 API**：

1. 建立 WebSocket 连接：`GET /api/agents/{id}/stream`（upgrade）
2. 发送：`{ "type": "message", "content": "用户输入" }`
3. 接收流式事件直到 `done`

### 4.4 Results Panel（第四栏，320px，可折叠）

#### 用户模式

```
┌─ Execution Results ─────────── [◀] ─┐
│                                      │
│  📊 本次对话统计                      │
│  ┌────────────────────────────────┐  │
│  │ Prompt tokens:     1,234       │  │
│  │ Completion tokens:   567       │  │
│  │ Total tokens:      1,801       │  │
│  │ Iterations:            3       │  │
│  └────────────────────────────────┘  │
│                                      │
│  🔧 工具调用记录                      │
│  ┌────────────────────────────────┐  │
│  │ http_request   GET  1.2s  ✅  │  │
│  │ memory_recall  -    0.3s  ✅  │  │
│  └────────────────────────────────┘  │
│                                      │
│  📈 Agent 运行状态                    │
│  ┌────────────────────────────────┐  │
│  │ Status: Running               │  │
│  │ Uptime: 5m 32s                │  │
│  │ Provider: openai/gpt-4o       │  │
│  └────────────────────────────────┘  │
└──────────────────────────────────────┘
```

| 区域 | 数据源 | 刷新时机 |
|------|--------|---------|
| Token 统计 | WebSocket `done` 事件的 `usage` 字段 | 每次对话完成 |
| 工具调用记录 | WebSocket `tool_call` + `tool_result` 事件 | 每次工具调用完成 |
| Agent 运行状态 | `GET /api/agents` 的 `status` 字段 | 每 30s + 操作后 |

**折叠/展开**：点击标题栏 [◀] 按钮折叠 Results Panel。折叠后显示一个小按钮 [▶] 在 Chat Panel 右边缘。

---

## 5. Models 视图（Provider 管理）

### 5.1 Provider 列表

```
┌──────────────────────────────────────────────────────────────┐
│  Models                                        [+ Add Key]  │
│                                                              │
│  ┌────────────────────────────────────────────────────────┐  │
│  │  🔑 OpenAI                                     ✅ 活跃  │  │
│  │  gpt-4o · gpt-4o-mini · gpt-3.5-turbo                 │  │
│  │  Key: sk-...4f2g                               [⚙️] [🗑]│  │
│  └────────────────────────────────────────────────────────┘  │
│                                                              │
│  ┌────────────────────────────────────────────────────────┐  │
│  │  🔑 Anthropic                                  ⚪ 未配置 │  │
│  │  点击配置 API Key                                      │  │
│  └────────────────────────────────────────────────────────┘  │
│                                                              │
│  ┌────────────────────────────────────────────────────────┐  │
│  │  🏠 Ollama (Local)                             ✅ 可用  │  │
│  │  qwen3:8b · llama3:8b                                  │  │
│  │  http://localhost:11434                          [⚙️]   │  │
│  └────────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────┘
```

**数据源**：

| 数据 | API |
|------|-----|
| 已配置的 API Key | `GET /api/vault/keys` |
| 可用 Provider 列表 | 前端硬编码（OpenAI / Anthropic / Google / Ollama / Azure） |

**`GET /api/vault/keys` 响应格式**：

```json
[
  {
    "provider": "openai",
    "created_at": "2026-04-27T10:00:00Z",
    "has_key": true
  }
]
```

**Provider 卡片交互**：

| 操作 | 行为 |
|------|------|
| [⚙️] 编辑 | 弹出编辑对话框，可更新 Key |
| [🗑] 删除 | 确认后调用 `DELETE /api/vault/keys/{provider}` |
| [+ Add Key] | 弹出添加对话框 |

**添加/编辑 API Key 对话框**：

```
┌─ Add API Key ────────────────────────────┐
│                                           │
│  Provider                                 │
│  ┌─────────────────────────────────────┐  │
│  │ OpenAI                        [▼]   │  │
│  └─────────────────────────────────────┘  │
│                                           │
│  API Key                                  │
│  ┌─────────────────────────────────────┐  │
│  │ sk-••••••••••••••••••••••••••••    │  │
│  └─────────────────────────────────────┘  │
│                                           │
│                [取消]  [保存]             │
└───────────────────────────────────────────┘
```

- Provider 下拉选择（OpenAI / Anthropic / Google / Azure）
- Ollama 不需要 API Key，只配置 base_url
- API Key 输入框：`type=password`，右侧有 [👁] 切换可见性
- 保存：`POST /api/vault/keys`（body: `{ "provider": "openai", "key": "sk-..." }`）

---

## 6. Settings 视图

### 6.1 页面结构

使用 Tab 布局：

```
┌──────────────────────────────────────────────────────────┐
│  Settings                                                │
│                                                          │
│  [Gateway] [Providers] [Vault] [Appearance] [General]   │
│  ─────────────────                                       │
│                                                          │
│  （Tab 内容区）                                           │
│                                                          │
└──────────────────────────────────────────────────────────┘
```

### 6.2 Gateway Tab

```
┌──────────────────────────────────────────────────────────┐
│  Gateway 连接                                            │
│                                                          │
│  地址                                                     │
│  ┌───────────────────────────────────────────────────┐   │
│  │ http://127.0.0.1:19876                           │   │
│  └───────────────────────────────────────────────────┘   │
│                                                          │
│  状态    🟢 Connected                                    │
│  版本    v0.5.0                                          │
│  运行时间 2h 15m                                         │
│                                                          │
│  [测试连接]                                              │
└──────────────────────────────────────────────────────────┘
```

| 字段 | 数据源 | 说明 |
|------|--------|------|
| 地址 | `tauri-plugin-store` 持久化 | 默认 `http://127.0.0.1:19876` |
| 状态 | `GET /health` | ok / degraded / unhealthy / disconnected |
| 版本 | `GET /health` 的 `version` 字段 | — |
| 运行时间 | `GET /api/status` | — |
| [测试连接] | 调用 `GET /health` | 成功显示 ✅，失败显示错误信息 |

### 6.3 Providers Tab

与 Models 视图（§5）相同内容，嵌入 Settings 中作为 Tab。

### 6.4 Vault Tab

```
┌──────────────────────────────────────────────────────────┐
│  API Key 管理                                            │
│                                                          │
│  ┌──────────────────────────────────────────────────┐    │
│  │ OpenAI    sk-...4f2g    2026-04-27    [编辑] [删] │    │
│  │ Anthropic Not set       -            [添加]      │    │
│  │ Ollama    (local)       -            [配置]      │    │
│  └──────────────────────────────────────────────────┘    │
│                                                          │
│  [+ Add API Key]                                         │
└──────────────────────────────────────────────────────────┘
```

Vault Tab 与 Models 视图共享同一组件（`<VaultKeyManager />`），API 相同。

### 6.5 Appearance Tab

```
┌──────────────────────────────────────────────────────────┐
│  外观设置                                                │
│                                                          │
│  主题                                                    │
│  ○ 浅色  ○ 深色  ○ 跟随系统                              │
│                                                          │
│  字体大小                                                │
│  ┌───┬───┬───┬───┬───┐                                  │
│  │ S │ M │ L │XL │XXL│                                  │
│  └───┴───┴───┴───┴───┘                                  │
│                                                          │
│  [恢复默认]                                              │
└──────────────────────────────────────────────────────────┘
```

| 设置 | 存储 | 说明 |
|------|------|------|
| 主题 | `tauri-plugin-store` | `light` / `dark` / `system` |
| 字体大小 | `tauri-plugin-store` | 对应 `--text-base` 的倍率：0.875 / 1.0 / 1.125 / 1.25 / 1.375 |

### 6.6 General Tab

```
┌──────────────────────────────────────────────────────────┐
│  通用设置                                                │
│                                                          │
│  日志级别                                                │
│  ┌───────────────────────────────────────────────────┐   │
│  │ info                                        [▼]   │   │
│  └───────────────────────────────────────────────────┘   │
│                                                          │
│  数据目录                                                │
│  ┌───────────────────────────────────────────────────┐   │
│  │ ~/.local/share/rollball                     [...]  │   │
│  └───────────────────────────────────────────────────┘   │
│                                                          │
│  关于                                                    │
│  Rollball Desktop v0.1.0                                │
│  Built with Tauri v2 + React 19                         │
└──────────────────────────────────────────────────────────┘
```

| 设置 | 存储 | 说明 |
|------|------|------|
| 日志级别 | `tauri-plugin-store` | trace / debug / info / warn / error |
| 数据目录 | 只读显示 | Gateway 配置的 `data_dir`，来自 `GET /api/config` |

---

## 7. 首次启动引导

### 7.1 流程概览

```
Step 1: 欢迎 ──→ Step 2: Gateway 连接 ──→ Step 3: API Key ──→ Step 4: 身份信息 ──→ Step 5: 安装 Agent
                                                                            │
                                                                  跳过 → 主界面
```

- 引导状态持久化：`tauri-plugin-store` 存储 `onboarding_completed: boolean`
- 已完成引导后不再显示，但可在 Settings → General 中重置
- 支持断点续传：记录当前步骤，下次打开从该步骤继续

### 7.2 Step 1: 欢迎

```
┌──────────────────────────────────────────────────────────────┐
│                                                              │
│                       🎉                                     │
│                                                              │
│              欢迎使用 Rollball                                │
│                                                              │
│        让我们快速配置你的 Agent 环境                           │
│                                                              │
│                                                              │
│                    [开始配置]                                 │
│                                                              │
│         已有配置？[跳过引导 →]                                │
└──────────────────────────────────────────────────────────────┘
```

- [跳过引导 →]：设置 `onboarding_completed = true`，进入主界面

### 7.3 Step 2: Gateway 连接

```
┌──────────────────────────────────────────────────────────────┐
│  Step 2 of 5                                                 │
│  ━━━━━━━━━━━○○○○                                            │
│                                                              │
│  连接 Gateway                                                │
│                                                              │
│  Gateway 地址                                                │
│  ┌───────────────────────────────────────────────────────┐   │
│  │ http://127.0.0.1:19876                                │   │
│  └───────────────────────────────────────────────────────┘   │
│                                                              │
│  状态: 🔍 正在检测...                                        │
│                                                              │
│                           [上一步]  [下一步]                 │
└──────────────────────────────────────────────────────────────┘
```

**交互逻辑**：

1. 页面加载后自动尝试连接 `GET /health`
2. 成功 → 状态显示 🟢 "Gateway 已连接"，[下一步] 可用
3. 失败 → 状态显示 🔴 "无法连接 Gateway"
   - 显示提示："请确认 Gateway 已启动：`rollball-gateway --daemon`"
   - [下一步] 禁用
   - 提供 [重试] 按钮

### 7.4 Step 3: API Key 配置

```
┌──────────────────────────────────────────────────────────────┐
│  Step 3 of 5                                                 │
│  ━━━━━━━━━━━━━━━━○○○                                        │
│                                                              │
│  配置 LLM Provider                                           │
│                                                              │
│  至少配置一个 Provider 才能与 Agent 对话                      │
│                                                              │
│  ┌───────────────────────────────────────────────────────┐   │
│  │  🔑 OpenAI                                           │   │
│  │  ┌─────────────────────────────────────────────────┐ │   │
│  │  │ sk-...                                          │ │   │
│  │  └─────────────────────────────────────────────────┘ │   │
│  └───────────────────────────────────────────────────────┘   │
│                                                              │
│  ┌───────────────────────────────────────────────────────┐   │
│  │  🏠 Ollama (本地，无需 Key)                     ✅    │   │
│  └───────────────────────────────────────────────────────┘   │
│                                                              │
│  [+ 添加其他 Provider]                                      │
│                                                              │
│                           [上一步]  [下一步]                 │
└──────────────────────────────────────────────────────────────┘
```

**交互逻辑**：

1. 检测本地是否有 Ollama 运行（`GET http://localhost:11434/api/tags`），有则自动标记为 ✅
2. OpenAI Key 输入后，调用 `POST /api/vault/keys` 验证并保存
3. 至少一个 Provider 可用（有 Key 或 Ollama 在线）时 [下一步] 可用
4. 可跳过（但主界面会提示"未配置 Provider"）

### 7.5 Step 4: 身份信息

```
┌──────────────────────────────────────────────────────────────┐
│  Step 4 of 5                                                 │
│  ━━━━━━━━━━━━━━━━━━━━○○                                     │
│                                                              │
│  身份信息                                                    │
│                                                              │
│  帮助 Agent 更好地了解你（必填项标 *）                        │
│                                                              │
│  称谓 *         ┌───────────────────┐                        │
│                 │ 小明              │                        │
│                 └───────────────────┘                        │
│                                                              │
│  语言 *         ┌───────────────────┐                        │
│                 │ 中文 (简体)    [▼]│                        │
│                 └───────────────────┘                        │
│                                                              │
│  时区 *         ┌───────────────────┐                        │
│                 │ Asia/Shanghai [▼] │                        │
│                 └───────────────────┘                        │
│                                                              │
│  城市 (选填)    ┌───────────────────┐                        │
│                 │ 北京              │                        │
│                 └───────────────────┘                        │
│                                                              │
│  职业 (选填)    ┌───────────────────┐                        │
│                 │                   │                        │
│                 └───────────────────┘                        │
│                                                              │
│                           [上一步]  [下一步]                 │
└──────────────────────────────────────────────────────────────┘
```

**交互逻辑**：

1. 必填字段（称谓、语言、时区）未填写时 [下一步] 禁用
2. [下一步] 点击后调用 `POST /api/system/identity`（如果 Gateway 支持），否则本地存储
3. 可跳过（跳过后使用默认值："User" / "中文" / "Asia/Shanghai"）

### 7.6 Step 5: 安装第一个 Agent

```
┌──────────────────────────────────────────────────────────────┐
│  Step 5 of 5                                                 │
│  ━━━━━━━━━━━━━━━━━━━━━━━━○                                  │
│                                                              │
│  安装你的第一个 Agent                                        │
│                                                              │
│  ┌───────────────────────────────────────────────────────┐   │
│  │  🌤  Weather Agent                                    │   │
│  │  查询天气信息，支持全球城市                             │   │
│  │                                        [安装]         │   │
│  └───────────────────────────────────────────────────────┘   │
│                                                              │
│  ┌───────────────────────────────────────────────────────┐   │
│  │  📅  Calendar Agent                                   │   │
│  │  日程管理和提醒                                        │   │
│  │                                        [安装]         │   │
│  └───────────────────────────────────────────────────────┘   │
│                                                              │
│  或者：                                                     │
│  [📁 从文件安装 .agent 包]                                   │
│                                                              │
│                           [上一步]  [完成 →]                 │
└──────────────────────────────────────────────────────────────┘
```

**交互逻辑**：

1. 示例 Agent 列表从 Gateway 内置路径读取（`examples/` 目录下的 .agent 包）
2. [安装]：调用 `POST /api/agents/install`，显示安装进度
3. [📁 从文件安装]：打开文件选择对话框
4. [完成 →]：无论是否安装了 Agent 都可点击，设置 `onboarding_completed = true`

---

## 8. 错误处理 UX

### 8.1 Toast 通知

所有非致命错误通过 Toast 通知显示：

```
┌─────────────────────────────────────────────┐
│ ❌ Failed to start Agent: Agent not found   │  ← 红色左边框
│                                    [✕]      │
└─────────────────────────────────────────────┘
```

| 属性 | 值 |
|------|-----|
| 位置 | 右下角，距底部 16px，距右边 16px |
| 自动消失 | 5s（成功通知）/ 8s（错误通知） |
| 堆叠 | 最多 3 条，新的推入，旧的提前消失 |
| 类型 | success（绿边）、error（红边）、warning（黄边）、info（蓝边） |

### 8.2 API 错误映射

| HTTP 状态码 | 用户提示 | 操作 |
|-------------|---------|------|
| 400 | "请求参数错误：{message}" | 无 |
| 404 | "未找到：{message}" | 无 |
| 409 | "操作冲突：{message}" | 无 |
| 500 | "服务器内部错误，请稍后重试" | [重试] 按钮 |
| 网络错误 | "无法连接 Gateway" | [设置] 按钮 |
| 超时 | "请求超时，请检查网络连接" | [重试] 按钮 |

### 8.3 加载状态

所有异步操作使用统一的加载态：

| 组件 | 加载态 | 骨架屏 |
|------|--------|--------|
| Agent 列表 | Spinner 居中 | 3 个灰色矩形条 |
| 消息流 | 无（增量加载） | 无 |
| Settings | Spinner 居中 | 无 |
| 对话框提交 | 按钮变为 Spinner + 禁用 | 无 |

---

## 9. 动画与过渡

| 场景 | 动画 | 时长 |
|------|------|------|
| Agent 列表项选中 | 背景色渐变 | 150ms ease |
| 聊天面板切换 Agent | 淡入淡出 | 250ms ease |
| Results Panel 折叠/展开 | 宽度过渡 | 250ms ease |
| Toast 出现/消失 | 从右滑入 + 淡入 / 淡出 | 250ms ease |
| 导航栏 Tab 切换 | 下划线滑动 | 150ms ease |
| 对话框打开/关闭 | 缩放 + 淡入 / 淡出 | 200ms ease |
| 流式打字光标 | 闪烁 | 1s infinite |
| Agent 状态变更 | 状态指示器颜色渐变 | 300ms ease |

**prefers-reduced-motion**：当系统开启减少动画，所有动画降级为即时切换（0ms）。

---

## 10. 键盘快捷键

| 快捷键 | 上下文 | 行为 |
|--------|--------|------|
| `Ctrl/Cmd + Enter` | 输入框 | 发送消息 |
| `Ctrl/Cmd + N` | 全局 | 打开安装 Agent 文件选择 |
| `Ctrl/Cmd + ,` | 全局 | 打开 Settings |
| `Ctrl/Cmd + Shift + D` | 全局 | 切换 Developer Mode（S2 启用）|
| `Escape` | 对话框 | 关闭对话框 |
| `Ctrl/Cmd + R` | 全局 | 刷新 Agent 列表 |

---

## 11. API 契约汇总

以下为 S1 用户模式所需的全部 Gateway HTTP API：

| 前端操作 | HTTP 方法 | 路径 | 请求体 | 响应 |
|---------|----------|------|--------|------|
| 健康检查 | GET | `/health` | — | `{ status, version, checks }` |
| 系统状态 | GET | `/api/status` | — | `{ ... }` |
| Agent 列表 | GET | `/api/agents` | — | `[{ agent_id, name, ... }]` |
| Agent 详情 | GET | `/api/agents/{id}` | — | `{ agent_id, name, manifest, ... }` |
| 安装 Agent | POST | `/api/agents/install` | `{ path }` | `{ agent_id }` |
| 卸载 Agent | DELETE | `/api/agents/{id}` | — | `{ success }` |
| 启动 Agent | POST | `/api/agents/{id}/start` | — | `{ status }` |
| 停止 Agent | POST | `/api/agents/{id}/stop` | — | `{ status }` |
| 发送消息 | WS | `/api/agents/{id}/stream` | `{ type: "message", content }` | 流式事件 |
| 获取配置 | GET | `/api/config` | — | `{ ... }` |
| 列出 Vault Key | GET | `/api/vault/keys` | — | `[{ provider, ... }]` |
| 添加 Vault Key | POST | `/api/vault/keys` | `{ provider, key }` | `{ success }` |
| 删除 Vault Key | DELETE | `/api/vault/keys/{provider}` | — | `{ success }` |
| 列出权限 | GET | `/api/agents/{id}/permissions` | — | `[...]` |
| 授予权限 | POST | `/api/agents/{id}/permissions/{perm}/grant` | — | `{ ... }` |
| 撤销权限 | DELETE | `/api/agents/{id}/permissions/{perm}` | — | `{ ... }` |

---

## 12. 状态管理（Zustand Store 设计）

```typescript
// --- Gateway Store ---
interface GatewayStore {
  status: 'connected' | 'disconnected' | 'error';
  health: HealthResponse | null;
  config: GatewayConfig | null;
  checkHealth: () => Promise<void>;
}

// --- Agent Store ---
interface AgentStore {
  agents: AgentInfo[];
  selectedAgentId: string | null;
  loading: boolean;
  fetchAgents: () => Promise<void>;
  selectAgent: (id: string) => void;
  installAgent: (path: string) => Promise<void>;
  uninstallAgent: (id: string) => Promise<void>;
  startAgent: (id: string) => Promise<void>;
  stopAgent: (id: string) => Promise<void>;
}

// --- Chat Store ---
interface ChatStore {
  messages: ChatMessage[];
  streamingMessageId: string | null;
  sending: boolean;
  wsConnection: WebSocket | null;
  connectStream: (agentId: string) => void;
  sendMessage: (content: string) => void;
  disconnectStream: () => void;
}

// --- Settings Store ---
interface SettingsStore {
  theme: 'light' | 'dark' | 'system';
  fontSize: number;
  gatewayUrl: string;
  logLevel: string;
  setTheme: (theme: string) => void;
  setFontSize: (size: number) => void;
}

// --- Onboarding Store ---
interface OnboardingStore {
  completed: boolean;
  currentStep: number;
  setCompleted: () => void;
  setStep: (step: number) => void;
}
```

---

## 13. 无障碍

| 规则 | 实现 |
|------|------|
| 键盘导航 | 所有交互元素可通过 Tab 聚焦，Enter/Space 激活 |
| ARIA 标签 | 导航栏 `role="navigation"`、Agent 列表 `role="list"`、消息流 `role="log"` |
| 对比度 | 文本对比度 ≥ 4.5:1（WCAG AA） |
| 焦点指示器 | 所有可聚焦元素显示可见的焦点环（2px 蓝色外框） |
| 屏幕阅读器 | 消息类型通过 `aria-label` 标注（"User message"、"Assistant message"、"Tool call: http_request"） |

---

## 14. 与设计文档的关系

| 文档 | 关系 |
|------|------|
| `docs/design/14-desktop-app.md` | 架构、技术选型、窗口管理 — 本文档在其基础上细化交互 |
| `docs/design/10-debug-protocol.md` | 开发者模式协议 — 本文档不覆盖，S2 阶段补充 |
| `docs/design/13-skill-system.md` | Skill 编辑器 — 本文档不覆盖，S3 阶段补充 |
| `docs/plan/plan-p5.md` | S1 任务定义 — 本文档是 S1 前端开发的实现依据 |
