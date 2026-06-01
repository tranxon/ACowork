# Phase 5: Desktop App + 开发框架 — 实施计划

> 版本：v1.2 | 更新日期：2026-05-06
> 前置条件：Phase 4（M15~M19）全部完成
> 预计周期：19~22 周
> 预计里程碑：M20~M25

---

## 背景与目标

### 已完成阶段回顾

| Phase | 主题 | 里程碑 | 状态 |
|-------|------|--------|------|
| Phase 1 | 基础框架 + LLM 交互 | M1~M4 | ✅ 完成 |
| Phase 2 | Grafeo 记忆 + System Agent + Intent + 多 Provider | M5~M9 | ✅ 完成 |
| Phase 3 | 权限框架 + WASM 沙箱 + Shell 安全 + 离线巩固 | M10~M14 | ✅ 完成 |
| Phase 4 | HTTP API + Permission IPC + Cron + RAG | M15~M19 | ✅ 完成（266 测试，0 失败）|

### Phase 5 核心目标

1. **Desktop App**：基于 Tauri v2 + React 的桌面客户端，作为用户与 Agent 交互的主界面
2. **Debug Protocol**：Agent Runtime DevMode 的完整实现，支持步进调试、消息编辑、快照回滚
3. **开发框架高级能力**：Skill 热加载、Provider 动态切换、录制回放引擎
4. **发布工具链**：Agent 克隆、发布检查、打包签名、分发
5. **Phase 4 遗留技术债务清偿**：13 项 P2 问题纳入 S5 处理

### 已有基础

| 模块 | 当前状态 | 说明 |
|------|---------|------|
| Runtime `--dev-mode` | CLI 参数 + 配置字段已存在 | `cli.rs` 有 `dev_mode: bool`，主循环仅有 TODO 注释 |
| Skills 模块 | `SkillDefinition` + parser + registry | 仅静态定义层（SKILL.md 解析），Grafeo 经验层未实现 |
| Gateway HTTP API | 完整实现 | Agent CRUD + Chat + Vault + Config + Permission + Cron |
| Gateway 克隆/发布 API | 未实现 | 缺少 `/api/agents/:id/clone` 和 `/api/agents/:id/publish/*` |
| Tauri Desktop App | 未创建 | `apps/` 目录为空 |
| Debug Protocol Server | 未实现 | 无 WebSocket 服务端、无 JSON-RPC 2.0 处理 |
| 录制回放引擎 | 未实现 | 无 JSONL 录制/回放逻辑 |

---

## 阶段划分

| 阶段 | 主题 | 任务数 | 预期测试 | 预计周期 | 状态 |
|------|------|--------|---------|---------|------|
| S1 | Desktop App 骨架 + 系统托盘 + 对话持久化 | 18 | 150 | 7 周 | ✅ 完成 |
| S2 | Debug Protocol + Session Actor 重构 | 13 | 101 | 5.5~6.5 周 | ✅ 完成 |
| ~~S3~~ | ~~开发框架高级能力~~ | ~~8~~ | ~~48~~ | ~~3~4 周~~ | ⏸️ 延后至 Phase 6 |
| S4 | 发布工具链 | 7 | 25 | 2 周 | ✅ 完成 |
| S5 | Phase 4 遗留技术债务 + 集成验证 | 10 | 50 | 3~4 周 | ⏳ 待开始 |
|| **合计** | | **35** | **326** | **12.5~13.5 周** | |

---

## S1：Desktop App 骨架 + 系统托盘 + 对话持久化（7 周，17 项任务）

Desktop App 基础设施搭建 + 对话持久化与 Session 机制，用户模式下的完整功能闭环。

**涉及 crate**：新增 `apps/rollball-desktop/`（Tauri 项目）、`rollball-runtime`（Session + Episode 提炼）、`rollball-gateway`（Session API + IPC 转发）、`rollball-memory`（record_distilled）、`rollball-grafeo`（Episode 写入）

### Wave A：项目初始化与 Gateway 通信（2 周，5 项）

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|----------|
| S1.1 Tauri v2 项目初始化 | `npm create tauri-app@latest` 创建项目骨架；React 19 + TypeScript + Vite + Tailwind CSS + shadcn/ui + Zustand；配置 `tauri.conf.json`（窗口最小尺寸 1024×600、默认 1200×800、单实例） | 3 | `cargo tauri dev` 启动成功、窗口尺寸正确 |
| S1.2 Gateway HTTP Client | Rust 后端 `gateway_client.rs`：封装 Gateway HTTP API（`reqwest` + `serde`）；Tauri Commands 暴露给前端（`gateway_health`、`list_agents`、`install_agent`、`start_agent`、`stop_agent`、`send_message`）；错误处理与重试逻辑 | 8 | 所有 Gateway API 可通过 Tauri Command 调用 |
| S1.3 四栏布局与 Agent 列表 | 前端实现：导航栏（48px）+ Agent 列表（240px）+ 聊天面板（弹性）+ 结果区（320px 可折叠）；Agent 列表组件（从 `list_agents` 加载、状态指示器、右键菜单）；响应式布局 | 5 | 四栏布局正确渲染、Agent 列表显示 |
| S1.4 聊天面板与流式对话 | 前端实现：消息流组件（user/assistant/tool 消息）；WebSocket 流式接收（`/api/agents/:id/stream`）；输入框（多行、快捷键）；工具调用展示（可展开/折叠）；Tauri Command: `send_message` | 8 | 消息收发正常、流式输出实时显示 |
| S1.5 系统托盘 | Rust 后端 `tray/` 模块：系统托盘图标 + 右键菜单（Show Dashboard / Agent Chat / Status / Quit）；Gateway 健康轮询（5s 间隔，连续 3 次失败降级到 30s）；关闭窗口隐藏到托盘（不退出）；`tauri-plugin-single-instance` | 6 | 托盘图标显示、健康状态指示、关闭不退出 |

### Wave B：设置页面与首次引导（2 周，4 项）

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|----------|
| S1.6 设置页面 | 前端实现：Gateway 连接配置 + 健康状态；Provider 管理（Vault API Key 增删改）；外观设置（主题亮/暗、字体大小）；通用设置（日志级别、数据目录） | 5 | 设置页面各功能可用 |
| S1.7 首次启动引导 | 前端实现：5 步引导流程（欢迎 → Gateway 连接 → API Key → 身份信息 → 安装第一个 Agent）；引导状态持久化（`tauri-plugin-store`）；身份信息通过 `POST /api/system/identity` 写入系统 Agent | 5 | 引导流程完成、身份信息写入成功 |
| S1.8 Agent 管理 UI 完善 | 前端实现：安装 Agent（文件选择 + 拖放 .agent）；卸载确认对话框；启动/停止按钮 + 状态转换；Agent 详情页面（manifest 信息） | 4 | Agent CRUD 操作通过 UI 完成 |
| S1.9 执行结果区（用户模式） | 前端实现：工具调用摘要（工具名、参数、耗时、状态）；**工具调用可展开/折叠的详细信息面板**（完整参数、返回值、执行时间）；**权限追溯信息**（哪个权限允许/拦截了此调用，PermissionGrant 来源）；Token 用量统计（prompt/completion/total）；当前 Agent 运行状态 | 5 | 结果区正确展示工具调用、Token 用量、权限追溯信息 |

### Wave C：记忆管理 + Skill 浏览器 + Tool Approval Gate（1 周，3 项）

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|----------|
| S1.10 记忆管理面板（用户模式） | **Gateway HTTP API**（新增）：<br>• `GET /api/agents/:id/memory/nodes` — 查询记忆节点列表（支持分页、类型过滤：Knowledge/Episodic/Procedural）<br>• `GET /api/agents/:id/memory/stats` — 记忆统计（节点数、存储大小、各类型占比）<br>• `DELETE /api/agents/:id/memory/nodes/:node_id` — 手动删除记忆节点<br>• `POST /api/agents/:id/memory/consolidate` — 触发离线巩固<br>**Desktop UI**：<br>• 记忆面板组件（`MemoryPanel.tsx`）：展示 Knowledge/Episodic/Procedural 节点列表<br>• 搜索和过滤（按类型、关键词、时间范围）<br>• 节点详情展示（内容、置信度、decay_score、上次访问时间）<br>• 手动标记为 Dormant / 删除操作 | 8 | 记忆 API 返回正确、UI 展示和过滤正常、删除/标记操作生效 |
| S1.11 Skill 浏览器（用户模式） | **Gateway HTTP API**（新增）：<br>• `GET /api/agents/:id/skills` — Skill 列表（名称、触发词、依赖工具、状态）<br>• `GET /api/agents/:id/skills/:name` — Skill 详情<br>• `GET /api/agents/:id/skills/:name/history` — Skill 执行历史<br>**Desktop UI**：<br>• Skill 列表组件（`SkillBrowser.tsx`）：展示当前 Agent 拥有的 Skill<br>• 每个 Skill 显示：名称、描述、触发词、依赖工具列表<br>• Skill 执行历史（成功/失败统计） | 6 | Skill API 返回正确、浏览器展示和过滤正常、历史统计准确 |
| S1.12 Tool Approval Gate 交互 | **Desktop UI**：<br>• 高风险工具调用时显示确认对话框（模态框），展示工具名、参数摘要、风险等级<br>• Shell 命令风险分级展示（Low/Medium/High 标签 + 颜色区分）<br>• 用户可选择：允许 / 拒绝 / 允许本次会话所有同类操作<br>• Tool 执行详情展开面板：参数、返回值、执行时间、权限信息<br>• 与 Gateway Permission IPC 链路对接：`POST /api/agents/:id/permissions/approve` 或 WebSocket 实时推送 | 6 | 高风险工具触发确认对话框、三种选择正确响应、权限信息准确展示 |

> **前置依赖**：S1.10 依赖 Gateway HTTP API 扩展（需 `/api/agents/:id/memory/*` 路由支持）；S1.11 依赖 Skill Registry 已注册 Skill 的查询接口；S1.12 依赖 Phase 3 已实现的 Permission IPC 和 Approval Gate 后端逻辑。

### Wave D：对话持久化与 Session 机制（2 周，6 项）

> 依赖设计文档：`15-conversation-persistence.md`

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|----------|
| S1.13 Session 机制与 ConversationWriter（Runtime 层） | **Runtime 层新增**：<br>• 新增 `conversation.rs`：`ConversationSession` 结构体，Session 生命周期管理（Created → Active → Idle → Ended）<br>• Session ID 生成：`{timestamp}_{short_uuid}` 格式（timestamp=YYYYMMDD，short_uuid=UUID v4 前 32 bit）<br>• `ConversationWriter`：`mpsc::channel` 单写入者架构（主循环 + 工具线程 → Sender → Writer 线程独占文件句柄）<br>• JSONL 文件写入：首行固定写入 `SessionMetadata`（`_type="session_meta"`），后续行追加 `ConversationLine`<br>• `ConversationLine` 支持 role：user / assistant / think / tool_call / tool_result / system<br>• 每次写入后 `flush()`，确保崩溃恢复<br>• Runtime 启动时异步扫描 `conversations/` 目录构建 session 列表（`tokio::spawn`，立即创建/恢复当前 session 不阻塞） | 8 | Agent 启动能创建 session，对话消息正确写入 JSONL 文件（首行元数据 + 消息行格式正确） |
| S1.14 Gateway Session API 与 IPC 转发 | **Gateway HTTP API**（新增/变更）：<br>• `GET /api/agents/{id}/conversations` — session 列表（IPC → Runtime 扫描首行元数据）<br>• `GET /api/agents/{id}/conversations/{session_id}/messages` — 分页加载消息（cursor + limit + direction）<br>• `POST /api/agents/{id}/conversations/new` — 创建新 session<br>• `GET /api/agents/{id}/conversations/latest` 变更：数据源从 Grafeo Episode 改为 IPC 转发 Runtime 读取 JSONL<br>• Gateway 不直接读取 JSONL 文件，所有 conversation API 通过 IPC 转发给 Runtime<br>• IPC 消息类型扩展：新增 `ConversationMessages` / `SessionList` 等请求/响应类型 | 6 | API 返回正确的 session 列表和分页消息，Gateway 不触碰 Agent 私有数据 |
| S1.15 Desktop App Session 选择器 UI | **Desktop UI**：<br>• 聊天框 Memory 按钮改为 Session 按钮<br>• Session 列表面板：最上方 Memory 入口（进入 Grafeo 记忆面板），下方 session 列表（按时间倒序）<br>• 每个 session 显示：title、message_count、status、恢复按钮<br>• 选中 session 后分页加载聊天记录（初始 50 条，向上滚动触发 `loadMore(cursor, direction=backward)`）<br>• 新建 session 功能<br>• `chatStore.ts` 适配新消息格式（支持 think / tool_call / tool_result / system 角色）<br>• think 内容默认折叠，点击展开 | 5 | 能切换 session 查看历史聊天记录，向上滚动加载更多消息，think 块可折叠/展开 |
| S1.16 Episode 提炼集成（上下文压缩触发 + Session 结束触发） | **Runtime 层新增**：<br>• 新增 `episode_distill.rs`：FIFO 裁剪时对被裁剪的历史消息做 LLM Episode 提炼（替代原每轮 `MemoryManager.record()`）<br>• Session 结束时对整个 session 做摘要 Episode 提炼<br>• 模型选择：从可用 LLM 列表中选 cost 最低的模型；无可用模型时降级为不提炼<br>• LLM 提炼输出结构化字段：summary / intent_type / decision / tool_summary / keywords<br>• Episode 写入 Grafeo（`consolidated=false`）<br>• `MemoryManager.record` → `record_distilled` 接口变更<br>• 提炼 offset 机制：记录已提炼到第几行，防止同一段对话被重复提炼 | 8 | 上下文压缩和 session 结束时能生成 Episode 节点，metadata 字段完整 |
| S1.17 Agent 打包对话数据隔离 | **PackageManager 打包扩展**：<br>• 打包时展示数据选择 checklist UI<br>• 默认勾选：manifest、prompts、skills、KnowledgeNode(Public)、ProceduralNode、AutobiographicalNode<br>• 默认不勾选：`conversations/`（JSONL 对话文件）、Episode、KnowledgeNode(Private)<br>• 始终排除：`memory/`（Grafeo 原始文件）、`workspace/`、`runtime/`、`*.log`、`*.tmp`<br>• 用户可手动调整，私有数据项旁提示 ⚠️ 隐私风险<br>• Grafeo 节点按类型过滤导出（非 raw DB 文件），仅打包两端节点均被选中的边<br>• 勾选含隐私数据的项后打包前弹出二次确认 | 6 | 打包产物按用户选择正确包含/排除数据，默认排除隐私项 |
| S1.18 端到端集成测试 | **覆盖 Wave D 全功能点的集成测试**（50 个用例，8 个模块分组）：<br>• A. Session 机制测试（6 项）：首次启动创建、重启恢复、重复创建防护、ID 格式验证、异步扫描性能、Session 结束元数据更新<br>• B. JSONL 写入测试（9 项）：user/assistant/tool_call/tool_result/think 写入、并发顺序、超长消息、损坏恢复、崩溃恢复<br>• C. Gateway Session API 测试（7 项）：session 列表、分页加载（首次/翻页/前向）、空 session、404、IPC 转发验证<br>• D. Desktop Session 选择器 UI 测试（7 项）：按钮展开、Memory 入口、session 切换刷新、新建切换、滚动加载、渐进加载<br>• E. Episode 提炼测试（8 项）：上下文压缩触发、Session 结束触发、cost 最低模型选择、降级不提炼、语义摘要、source_session_id、offset 去重、consolidated 标记<br>• F. Agent 打包数据隔离测试（5 项）：默认包含/排除验证、手动勾选、UI 大小和隐私提示、安装后隔离验证<br>• G. 端到端全链路测试（3 项）：完整对话链路、长对话压缩链路、多 Agent 隔离<br>• H. 边界情况与异常测试（5 项）：目录自动创建、只读权限、磁盘不足、特殊字符、并发安全<br>详见下方 S1.18 详细测试用例 | 50 | 全部 50 个测试用例通过，覆盖 Wave D 所有功能点 |

#### S1.18 端到端集成测试（详细用例）

**覆盖范围**：Wave D（S1.13~S1.17）全部功能点的端到端集成验证，共 50 个测试用例，按 8 个模块分组。

预期测试数：50

##### A. Session 机制测试（6 项）

| 编号 | 测试用例 | 前置条件 | 操作步骤 | 预期结果 |
|------|---------|---------|---------|----------|
| T1 | Agent 首次启动自动创建 session | Agent 已安装，conversations/ 目录为空 | 1. 启动 Agent Runtime<br>2. 发送第一条用户消息 | 1. conversations/ 目录下生成 `{timestamp}_{short_uuid}.jsonl` 文件<br>2. JSONL 文件第一行为 SessionMetadata，`_type="session_meta"`，`status="active"`<br>3. session_id 格式为 `{YYYYMMDD}_{8位hex}` |
| T2 | Agent 重启后恢复最新 session | Agent 有一个 status=active 的 session | 1. 停止 Agent Runtime<br>2. 重新启动 Agent Runtime<br>3. 检查当前 session | 1. 不创建新 session<br>2. 恢复原有的 active session<br>3. JSONL 文件首行元数据 status 仍为 "active" |
| T3 | 多次启动停止后 session 文件不重复创建 | Agent 已有 session 文件 | 1. 启动 Agent<br>2. 停止 Agent<br>3. 再次启动 Agent<br>4. 重复 3 次<br>5. 检查 conversations/ 目录 | 1. 目录下仍只有 1 个 session JSONL 文件<br>2. 未产生多余或重复的 session 文件 |
| T4 | Session ID 格式验证 | 无 | 1. 创建多个 session<br>2. 检查文件名和首行 session_id 字段 | 1. 格式为 `{YYYYMMDD}_{8位hex}`<br>2. 同一天创建的不同 session，timestamp 相同但 short_uuid 不同<br>3. 按文件名字典序排列即为时间倒序 |
| T5 | 异步扫描 conversations 目录性能 | conversations/ 目录下有 100+ 个 JSONL 文件 | 1. 启动 Agent Runtime<br>2. 测量从启动到可接收消息的耗时<br>3. 测量后台扫描完成时间 | 1. Agent 启动不阻塞（可立即对话）<br>2. 启动耗时与 session 数量无关<br>3. 后台扫描完成后 session 列表可用 |
| T6 | Session 结束后元数据正确更新 | Agent 有一个 active session | 1. 调用 session end 操作（超时/主动关闭）<br>2. 读取 JSONL 文件首行 | 1. 首行 `status` 更新为 "ended"<br>2. 首行 `ended_at` 字段非空，为 ISO 8601 时间戳<br>3. `message_count` 与实际消息行数一致 |

##### B. JSONL 写入测试（9 项）

| 编号 | 测试用例 | 前置条件 | 操作步骤 | 预期结果 |
|------|---------|---------|---------|----------|
| T7 | 用户消息正确写入 | Agent 有 active session | 1. 发送用户消息 "帮我分析项目"<br>2. 读取 JSONL 文件 | 1. 文件末尾新增一行<br>2. `role="user"`，`content="帮我分析项目"`<br>3. `id` 为 UUID v4，`ts` 为 ISO 8601 毫秒精度 |
| T8 | Assistant 响应正确写入 | Agent 有 active session，已发送用户消息 | 1. 等待 LLM 返回完整响应<br>2. 读取 JSONL 文件 | 1. 新增 `role="assistant"` 行<br>2. `content` 包含完整 markdown 文本<br>3. `metadata` 含 `model`、`provider`、`token_count`、`duration_ms` |
| T9 | Tool call 正确写入 | Agent 有 active session，LLM 返回 tool_calls | 1. LLM 返回含 tool_call 的响应<br>2. 读取 JSONL 文件 | 1. 每个 tool_call 写入独立行，`role="tool_call"`<br>2. `metadata` 含 `tool_name` 和 `tool_call_id`<br>3. `content` 为工具参数 JSON 字符串 |
| T10 | Tool result 正确写入 | tool_call 行已写入 | 1. 工具执行完成<br>2. 读取 JSONL 文件 | 1. 新增 `role="tool_result"` 行<br>2. `metadata.tool_call_id` 与对应 tool_call 行一致<br>3. `metadata` 含 `tool_name`、`success`、`duration_ms` |
| T11 | Think 内容正确写入 | LLM 返回含 `antha` 标签的响应 | 1. LLM 返回 think + 正文<br>2. 读取 JSONL 文件 | 1. 先写入 `role="think"` 行（think 标签内容）<br>2. 后写入 `role="assistant"` 行（正文内容）<br>3. think 行 `metadata` 含 `model` 字段 |
| T12 | 并发工具执行时消息顺序正确 | Agent 调用多个并行工具 | 1. 触发并行工具执行（如同时读 3 个文件）<br>2. 读取 JSONL 文件 | 1. 所有 tool_call 行先于所有 tool_result 行<br>2. 消息行无交错或截断<br>3. Channel 单写入者保证顺序性 |
| T13 | 超长消息正确写入不截断 | Agent 有 active session | 1. 发送 >100KB 的用户消息<br>2. 接收 LLM >100KB 的响应<br>3. 读取 JSONL 文件 | 1. 用户消息完整写入，无截断<br>2. Assistant 响应完整写入，无截断<br>3. 两行 JSON 均可正确解析 |
| T14 | JSONL 文件损坏恢复 | Agent 有含多行消息的 JSONL 文件 | 1. 手动修改 JSONL 文件中间某行使其 JSON 无效<br>2. 调用 read_jsonl 读取文件 | 1. 损坏行被跳过，记录警告日志<br>2. 损坏行前后的消息正常读取<br>3. 返回的消息列表按正确顺序排列 |
| T15 | 进程崩溃后 JSONL 文件完整可读 | Agent 有 active session，已写入多条消息 | 1. 写入若干消息<br>2. 模拟进程崩溃（kill 进程）<br>3. 重新启动后读取 JSONL 文件 | 1. 已写入的消息行完整可读<br>2. 最多丢失崩溃时正在写入的一行<br>3. 文件其余部分不受影响 |

##### C. Gateway Session API 测试（7 项）

| 编号 | 测试用例 | 前置条件 | 操作步骤 | 预期结果 |
|------|---------|---------|---------|----------|
| T16 | GET /sessions 返回正确列表 | Agent 有 3+ 个 session（含 active 和 ended） | 1. 调用 `GET /api/agents/{id}/conversations`<br>2. 检查响应 | 1. 返回 200<br>2. sessions 列表按时间倒序排列<br>3. 每个 session 含 session_id、title、message_count、status 字段 |
| T17 | 分页加载：首次加载最新 50 条 | Agent 有 60+ 条消息的 session | 1. 调用 `GET /api/agents/{id}/conversations/{sid}/messages`（无 cursor）<br>2. 检查响应 | 1. 返回最新 50 条消息<br>2. `has_more=true`<br>3. `cursor` 为最早一条消息的 ID |
| T18 | 分页加载：cursor 向上翻页 | 已执行 T17，有 cursor 值 | 1. 用 T17 返回的 cursor 调用 `direction=backward`<br>2. 检查响应 | 1. 返回更早的消息<br>2. `has_more` 根据剩余消息量正确设置<br>3. 无重复消息 |
| T19 | 分页加载：direction=forward | 已知某条消息 ID | 1. 用 cursor 和 `direction=forward` 请求<br>2. 检查响应 | 1. 返回比 cursor 更新的消息<br>2. 消息按时间正序排列 |
| T20 | 空 session 返回空列表 | 创建新 session，未发送任何消息 | 1. 调用 `GET /api/agents/{id}/conversations/{sid}/messages` | 1. 返回 200<br>2. `messages=[]`<br>3. `has_more=false` |
| T21 | 不存在的 session_id 返回 404 | 无 | 1. 调用 `GET /api/agents/{id}/conversations/nonexistent_id/messages` | 1. 返回 404<br>2. 错误信息包含 "Session not found" |
| T22 | IPC 转发验证 | Agent Runtime 正在运行 | 1. 调用 conversation API<br>2. 检查 Gateway 不直接读 JSONL 文件<br>3. 确认请求通过 IPC 转发 | 1. Gateway 日志显示 IPC 请求转发<br>2. 无直接文件 I/O 操作<br>3. Runtime 日志显示收到 IPC 请求并返回数据 |

##### D. Desktop Session 选择器 UI 测试（7 项）

| 编号 | 测试用例 | 前置条件 | 操作步骤 | 预期结果 |
|------|---------|---------|---------|----------|
| T23 | Session 按钮点击展开列表 | Desktop App 已连接 Agent | 1. 点击聊天框 Session 按钮 | 1. Session 列表面板展开<br>2. 面板包含 session 列表 |
| T24 | Session 列表最上方显示 Memory 入口 | Session 列表面板已展开 | 1. 查看面板顶部 | 1. 最上方有 Memory 入口行<br>2. 显示 "Memory" 或图标<br>3. 右侧有进入箭头 |
| T25 | 点击 Memory 入口进入 Grafeo 记忆面板 | Session 列表面板已展开 | 1. 点击 Memory 入口 | 1. 切换到 Grafeo 记忆面板<br>2. 显示记忆节点列表（Knowledge/Episodic/Procedural） |
| T26 | 选中不同 session 后聊天记录刷新 | Agent 有 2+ 个 session | 1. 在 Session 列表中点击另一个 session<br>2. 等待加载完成 | 1. 聊天面板清空并加载新 session 的消息<br>2. 消息内容与 JSONL 文件一致 |
| T27 | 新建 session 后自动切换 | Agent 有一个 active session | 1. 点击"新建对话"<br>2. 等待创建完成 | 1. 自动切换到新 session<br>2. 旧 session 状态变为 "ended"<br>3. 聊天面板清空，显示空对话 |
| T28 | 向上滚动触发加载更多 | 当前 session 消息数 > 50 | 1. 滚动到聊天面板顶部<br>2. 触发 loadMore | 1. 加载更早的消息<br>2. 消息追加到列表顶部<br>3. 加载指示器显示后消失 |
| T29 | Session 列表渐进加载 | Agent 有 100+ 个 session | 1. 启动 Desktop App<br>2. 立即查看 Session 列表<br>3. 等待后台扫描完成 | 1. 启动时仅显示当前 session<br>2. 后台扫描完成后列表逐步填充<br>3. 不阻塞 Agent 对话 |

##### E. Episode 提炼测试（8 项）

| 编号 | 测试用例 | 前置条件 | 操作步骤 | 预期结果 |
|------|---------|---------|---------|----------|
| T30 | 上下文压缩触发时生成 Episode | Agent 配置了较小的 context window，对话即将触发 FIFO 裁剪 | 1. 持续对话直到触发上下文压缩<br>2. 检查 Grafeo 中新增的 Episode | 1. 新增 Episode 节点<br>2. `consolidated=false`<br>3. `metadata.session_scope="trimmed"` |
| T31 | Session 结束时生成 Session 级 Episode | Agent 有含多轮对话的 session | 1. 结束当前 session<br>2. 检查 Grafeo 中新增的 Episode | 1. 新增 Episode 节点<br>2. `metadata.session_scope="full_session"`<br>3. 内容为全局摘要（核心主题 + 关键决策 + 工具统计） |
| T32 | Episode 使用 cost 最低的可用模型提炼 | Agent 配置了多个 LLM 模型，cost 不同 | 1. 触发 Episode 提炼<br>2. 检查 LLM 调用日志 | 1. 提炼调用使用 cost 值最低的模型<br>2. 非 Agent 默认对话模型（除非它也是最便宜的） |
| T33 | 所有模型不可用时降级不提炼 | 断开所有 LLM 连接或耗尽配额 | 1. 触发上下文压缩<br>2. 检查 Grafeo 和错误日志 | 1. 不抛出错误或 panic<br>2. Grafeo 无新增 Episode<br>3. 日志记录降级信息，等下次触发重试 |
| T34 | Episode 内容为 LLM 语义摘要 | Episode 已生成 | 1. 读取 Grafeo 中 Episode 节点<br>2. 对比原始 JSONL 对话 | 1. Episode.content 长度远小于原始对话（100~300 字符 vs 数千字符）<br>2. 内容包含 summary/intent_type/decision/tool_summary/keywords 结构化字段<br>3. 非原始对话逐字复制 |
| T35 | Episode metadata 包含 source_session_id | Episode 已生成 | 1. 读取 Episode.metadata | 1. `source_session_id` 字段非空<br>2. 值与生成该 Episode 的 session_id 一致<br>3. 可通过此字段溯源到原始 JSONL 文件 |
| T36 | 重复提炼防护：offset 机制 | 已执行过一次 FIFO 裁剪提炼 | 1. 再次触发上下文压缩<br>2. 检查提炼 offset 和新 Episode 内容 | 1. 新提炼从 offset 位置开始，不重复处理已提炼的消息<br>2. 新 Episode 内容不与之前的 Episode 重复 |
| T37 | consolidated 标记正确 | 已生成 Episode | 1. 检查新生成 Episode 的 consolidated 字段<br>2. 模拟离线巩固处理<br>3. 再次检查 | 1. 新 Episode `consolidated=false`<br>2. 巩固处理后 `consolidated=true`<br>3. 已巩固的 Episode 不会被重复处理 |

##### F. Agent 打包数据隔离测试（5 项）

| 编号 | 测试用例 | 前置条件 | 操作步骤 | 预期结果 |
|------|---------|---------|---------|----------|
| T38 | 默认打包包含正确项 | Agent 有完整数据（conversations、Grafeo 节点等） | 1. 使用默认选项（PackageOptions::default）打包<br>2. 解压 .agent 文件检查内容 | 1. 包含 manifest.toml、prompts/、skills/<br>2. 包含 KnowledgeNode(Public)、ProceduralNode、AutobiographicalNode<br>3. 不包含 conversations/、Episode、KnowledgeNode(Private) |
| T39 | 默认打包排除隐私项 | 同 T38 | 1. 使用默认选项打包<br>2. 检查 .agent 文件 | 1. 不包含 conversations/ 目录<br>2. 不包含 Episode 节点<br>3. 不包含 KnowledgeNode(Private) 节点<br>4. 不包含 memory/、workspace/、runtime/、*.log、*.tmp |
| T40 | 用户手动勾选私有数据后正确包含 | 同 T38 | 1. 设置 `include_conversations=true`、`include_episodes=true`、`include_private_knowledge=true`<br>2. 打包并解压检查 | 1. 包含 conversations/ 目录下的 JSONL 文件<br>2. 包含 Episode 节点<br>3. 包含 KnowledgeNode(Private) 节点 |
| T41 | 打包 UI 显示大小和隐私提示 | Desktop App 已打开，Agent 有数据 | 1. 打开打包 checklist UI<br>2. 检查各项显示 | 1. 每项数据旁显示大小<br>2. 隐私项旁显示 ⚠️ 提示<br>3. 勾选隐私项后弹出二次确认 |
| T42 | 打包产物安装后隔离验证 | 已生成 .agent 包（含/不含 conversations） | 1. 安装 .agent 包到新环境<br>2. 检查目录结构 | 1. conversations/ 目录为空（默认打包）<br>2. Grafeo 仅包含勾选类型的节点<br>3. 无 memory/ 原始文件、无 workspace/、无 *.log |

##### G. 端到端全链路测试（3 项）

| 编号 | 测试用例 | 前置条件 | 操作步骤 | 预期结果 |
|------|---------|---------|---------|----------|
| T43 | 完整对话链路 | Desktop App + Gateway + Agent Runtime 均已启动 | 1. 发送消息 "你好"<br>2. 等待 Agent 响应<br>3. 切换到其他 Agent<br>4. 切回原 Agent<br>5. 通过 Session API 加载历史 | 1. JSONL 文件包含 user 和 assistant 行<br>2. 切回后聊天记录完整恢复<br>3. 消息内容与发送时完全一致 |
| T44 | 长对话链路 | 同 T43 | 1. 发送 50+ 轮对话<br>2. 观察上下文压缩触发<br>3. 检查 Grafeo 和 JSONL 文件 | 1. 上下文压缩自动触发<br>2. Grafeo 新增 Episode（consolidated=false）<br>3. JSONL 文件包含全部 50+ 轮对话消息<br>4. Episode 内容为语义摘要而非原文 |
| T45 | 多 Agent 隔离 | 两个 Agent（A 和 B）均已安装并运行 | 1. 在 Agent A 中对话<br>2. 在 Agent B 中对话<br>3. 检查 Agent A 的 session/JSONL/Episode<br>4. 检查 Agent B 的 session/JSONL/Episode | 1. Agent A 的 session 列表不含 Agent B 的 session<br>2. Agent A 的 JSONL 文件不含 Agent B 的消息<br>3. Agent A 的 Grafeo 不含 Agent B 的 Episode<br>4. 反之亦然 |

##### H. 边界情况与异常测试（5 项）

| 编号 | 测试用例 | 前置条件 | 操作步骤 | 预期结果 |
|------|---------|---------|---------|----------|
| T46 | conversations 目录不存在时自动创建 | 首次安装 Agent，workspace 下无 conversations/ | 1. 启动 Agent Runtime<br>2. 检查 workspace 目录 | 1. conversations/ 目录自动创建<br>2. 新 session JSONL 文件正确生成<br>3. 无错误或 panic |
| T47 | JSONL 文件权限只读 | Agent 有 active session | 1. 将 JSONL 文件设为只读<br>2. 发送新消息 | 1. 写入失败记录错误日志<br>2. Agent 主循环不崩溃<br>3. 对话功能降级但不断线 |
| T48 | 磁盘空间不足 | Agent 有 active session | 1. 模拟磁盘满<br>2. 发送新消息触发写入 | 1. 写入失败记录错误日志<br>2. 不产生部分写入的损坏行<br>3. Agent 不崩溃，可优雅降级 |
| T49 | 极长 session_id 或特殊字符处理 | 无 | 1. 手动在 conversations/ 目录创建含特殊字符的 .jsonl 文件<br>2. 触发目录扫描 | 1. 扫描跳过无效文件名，记录警告<br>2. 不影响正常 session 的读取和列表 |
| T50 | 多客户端并发连接安全 | Agent Runtime 正在运行 | 1. 从两个 Desktop 客户端同时连接同一 Agent<br>2. 同时发送消息 | 1. Channel 单写入者保证 JSONL 文件写入无竞争<br>2. 消息行无交错<br>3. 无数据丢失或重复 |

> **前置依赖**：S1.13 依赖 `15-conversation-persistence.md` 设计文档；S1.14 依赖 S1.13（Runtime Session 机制就绪）；S1.15 依赖 S1.14（Gateway Session API 就绪）；S1.16 依赖 S1.13（ConversationSession 就绪）和 Grafeo Episode 写入接口；S1.17 依赖 S4.3 打包 API 和 Grafeo 节点查询接口；S1.18 依赖 S1.13~S1.17 全部完成（集成测试需所有功能点实现后验证）。

**里程碑 M20：Desktop App 用户模式可用** — 安装 Agent → 对话 → 查看结果 → 设置管理 → 记忆浏览 → Skill 查看 → Tool 审批 → Session 切换 → 对话持久化 → Episode 提炼

---

## S2：Debug Protocol 实现（4~5 周，9 项任务）

Agent Runtime DevMode 的完整实现，包含 Debug Protocol Server、执行控制、状态查询、快照机制。

**涉及 crate**：`rollball-runtime`（新增 `debug/` 模块）、`rollball-gateway`（新增 DevMode 启动参数）

### S2 架构决策：Session Actor 多会话并发模型

**问题发现过程**：最初发现 `run_gateway_loop()` 在 `agent_loop.run()` 处同步阻塞，导致 `continue_execution` 死锁。经过编译验证（`select_borrow_check_test.rs`），发现 `tokio::select!` 方案存在 `&mut agent_loop` 和 `&context_builder` 借用冲突（E0499 / E0502）。进一步分析发现，这些借用冲突的根因是 **AgentLoop 混合了 Agent 身份和 Session 状态**，导致单 Session 模型无法支持多 Session 并发——而这是其他 Agent 应用（ChatGPT、Claude 等）的基本能力。

**核心原则**：
1. **消息路由永远不应该被 Agent 执行阻塞**
2. **每个 Session 是独立的执行实体，互不阻塞**
3. **前端驱动 Session 选择（selectedSession），后端不维护 currentSession**

> **关联设计文档**：
> - Session Actor 详细架构：`15-conversation-persistence.md` §1.7
> - Agent 运行时主循环（Episode 触发点标注）：`03-agent-runtime.md` §2
> - Session 管理 IPC 消息 Proto 定义：`06-communication.md` §1.5
> - Token 预算分配策略：`15-conversation-persistence.md` §1.8
> - JSONL 安全保证（轮转事务性/并发读写/offset）：`15-conversation-persistence.md` §1.9

#### 架构：AgentCore + SessionState 分离 + Session Actor

```
Gateway Message Loop（纯路由，永不阻塞）
  │
  ├── chat_message(session_id=X)    → route to sessions[X].inbound_tx
  ├── continue_execution(session_id) → route to sessions[X].inbound_tx
  ├── create_session               → SessionManager::create() → spawn SessionTask
  ├── LLMConfigDelivery            → broadcast to all SessionTasks
  └── model_switch(session_id)     → route to sessions[X].inbound_tx
       │
       ▼
  ┌──────────────────────────────────────────┐
  │ SessionTask A        SessionTask B       │
  │  ├── SessionState     ├── SessionState   │
  │  │   ├── history       │   ├── history    │
  │  │   ├── conversation  │   ├── conversation│
  │  │   ├── model_override│   ├── model_override│
  │  │   └── loop_detector │   └── loop_detector│
  │  └── inbound_rx       └── inbound_rx     │
  │       │                    │             │
  │   独立 LLM 调用         独立 LLM 调用    │
  │   独立工具执行         独立工具执行      │
  └──────────────────────────────────────────┘
                    │
              共享 AgentCore (Arc)
              ├── provider: Arc<dyn Provider>
              ├── tools: Vec<Arc<dyn Tool>>
              ├── manifest: AgentManifest
              ├── budget_guard: BudgetGuard
              ├── gateway_model_capabilities
              └── max_output_tokens_limit
```

**关键结构体重构**：

```rust
/// Per-agent 共享状态（跨所有 Session）
struct AgentCore {
    provider: Arc<dyn Provider>,
    tools: Vec<Arc<dyn Tool>>,
    manifest: AgentManifest,
    budget_guard: BudgetGuard,
    gateway_model_capabilities: HashMap<String, ModelCapabilitiesInfo>,
    max_output_tokens_limit: u64,
    on_chunk: mpsc::Sender<ChunkEvent>,
}

/// Per-session 独立状态（完全隔离）
struct SessionState {
    session_id: String,
    history: HistoryManager,
    conversation: Option<ConversationSession>,
    loop_detector: LoopDetector,
    model_override: Option<String>,     // per-session 模型选择
    iteration_count: usize,
    token_usage: TokenUsage,
}

/// Session Actor：每个 Session 独立的 tokio task
async fn session_task(
    core: Arc<AgentCore>,
    state: SessionState,
    mut inbound_rx: mpsc::Receiver<SessionMessage>,
) {
    loop {
        tokio::select! {
            msg = inbound_rx.recv() => {
                match msg {
                    ChatMessage { content } => state.run(&core, &content).await,
                    ContinueExecution { .. } => state.continue_execution().await,
                    Interrupt { .. } => state.interrupt(),
                    ModelSwitch { model } => state.model_override = Some(model),
                    DebugContinue { step } => state.debug_continue(step).await,
                }
            }
        }
    }
}
```

**Gateway 消息循环重构**（从阻塞调用变为纯路由）：

```rust
// 之前：阻塞在 agent_loop.run()
match agent_loop.run(&content, &context_builder).await { ... }

// 之后：纯路由，永不阻塞
loop {
    match grpc_client.recv_message().await {
        Some(msg) => route_message(msg, &session_manager).await,
        None => { /* reconnect */ }
    }
}
```

#### 前端模型：selectedSession 替代 currentSession

```
旧模型：currentSession（后端驱动）
  后端："当前活跃 Session 是 X，所有消息发给 X"
  切换：前端请求切换 → 后端暂停旧的 → 启动新的 → 前端更新

新模型：selectedSession（前端驱动）
  后端："所有 Session 都在独立运行，不管你看哪个"
  前端："我选择看 Session X，显示它的消息流"
  切换：前端切换 selectedSessionId → 订阅新 Session 的流 → 完成
```

前端状态模型：

```typescript
// 之前
interface ChatStore {
  currentSessionId: string;
  messages: ChatMessage[];
  isStreaming: boolean;
}

// 之后
interface ChatStore {
  selectedSessionId: string;       // 前端选择查看的 Session
  sessions: Map<string, {         // 所有 Session 独立状态
    messages: ChatMessage[];
    isStreaming: boolean;
    model: string;                 // per-session 模型
  }>;
}
```

WebSocket 流复用：单个 WebSocket 连接，所有 Session 事件带 `session_id`，前端按 `selectedSessionId` 过滤显示。

#### 配置作用域矩阵

| 配置项 | 作用域 | 变更方式 | 影响范围 |
|--------|--------|---------|--------|
| 可用模型列表 | Agent | 设置页添加/删除 Provider | 所有 Session 立即可选 |
| 当前使用模型 | Session | 聊天面板模型选择器 | 仅 selectedSession |
| Provider API Key | Agent | 设置页 | 所有 Session 共享 |
| Workspace 目录 | Agent | 不可运行时变更 | 所有 Session 共享 |
| Workspace 上下文焦点 | Session（隐式） | 由对话历史和 Grafeo 检索自然决定 | 每个 Session 不同 |
| 工具权限 | Agent | manifest 声明 | 所有 Session 共享 |
| Token 预算 | Agent | 设置页 | 所有 Session 共享（共享预算池） |
| 对话历史 | Session | — | 独立 |
| Token 用量统计 | Session | — | 独立 |
| 迭代计数器 | Session | — | 独立 |

#### Debug Protocol 覆盖

Session Actor 模型下，Debug 协议自然适配：

| 需求 | 覆盖方式 |
|------|---------|
| 断点暂停后 Continue/Step | `session.inbound_tx.send(DebugContinue)` → SessionTask 处理 |
| 暂停期间读取状态 | Session 主动上报 `ChunkEvent::DebugPaused` 快照 |
| 暂停期间修改状态 | `SessionMessage::DebugModify` → SessionTask 处理 |
| 运行时设置断点 | `SessionMessage::SetBreakpoint` → SessionTask 处理 |
| 迭代限制 Continue | `SessionMessage::ContinueExecution` → SessionTask 处理 |
| 跨 Session 调试 | 可同时暂停多个 Session，独立步进 |

#### 方案演进记录

| 版本 | 方案 | 结论 |
|------|------|------|
| v1 | `tokio::select!` + 分类路由 | 编译验证发现 E0499/E0502 借用冲突，需分类路由绕过 |
| v2 | `tokio::select!` + inbound_tx 转发 + Queue+Interrupt | 可行但复杂，Session 切换仍需中断 |
| **v3** | **Session Actor 模型** | **根本性重构，消除所有借用冲突，支持多 Session 并发** |

编译验证代码：`rollball-runtime/tests/select_borrow_check_test.rs`（保留作为借用模式参考）

### Wave 0：Session Actor 重构（1~1.5 周，3 项） — S2 前置任务

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|----------|
| S2.0a AgentCore + SessionState 分离 | 从 `AgentLoop` 提取 `AgentCore`（共享状态：provider、tools、manifest、budget_guard）和 `SessionState`（per-session 状态：history、conversation、loop_detector、model_override）；`AgentLoop` 重构为持有 `Arc<AgentCore>` + `SessionState`；所有现有测试通过（行为不变） | 12 | 现有 266 测试全部通过、AgentCore/SessionState 结构体编译正确 |
| S2.0b SessionTask + SessionManager | 新增 `session/` 模块：`SessionTask`（每个 session 一个 tokio task，通过 `inbound_rx` 接收 `SessionMessage`）；`SessionManager`（管理 session 创建/销毁/查找，`HashMap<String, SessionHandle>`）；`SessionHandle`（inbound_tx + JoinHandle + on_chunk_sender） | 10 | 多 Session 并发运行互不阻塞、Session 创建/销毁正确 |
| S2.0c Gateway 消息循环重构 | `run_gateway_loop()` 从阻塞调用 `agent_loop.run()` 改为纯路由模式；消息按 `session_id` 路由到对应 `SessionHandle`；`chat_message` 包含 `session_id` 字段；`LLMConfigDelivery` 广播到所有 SessionTask；移除 `current_session_id` 追踪逻辑 | 8 | Gateway loop 不再阻塞、多 Session 消息正确路由 |

### Wave A：Debug Protocol Server + 执行控制（2 周，4 项）

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|----------|
| S2.1 Debug Protocol Server | 新增 `rollball-runtime/src/debug/server.rs`：WebSocket 服务端（`tokio-tungstenite`，`ws://127.0.0.1:19877`）；JSON-RPC 2.0 消息解析/响应；DevMode 检测（`--dev-mode` 启动时才监听）；连接管理（单客户端，后续连接拒绝） | 8 | WebSocket 连接建立/断开、JSON-RPC 消息收发 |
| S2.2 执行控制（Pause/Step/Resume/Stop） | 修改主循环 `loop_.rs`：引入 `DebugController`（`Arc<Mutex<DebugState>>`）；Pause 时主循环 `tokio::sync::watch` 阻塞等待 Resume；Step 执行一步后自动 Pause；Stop 终止当前对话；`onStep` 事件推送 | 10 | 执行控制命令正确响应、主循环暂停/恢复 |
| S2.3 状态查询 + 断点 | `debugger.getState`：返回当前迭代、Phase、消息列表、快照 IDs、断点、Token 用量；`debugger.setBreakpoint`/`removeBreakpoint`/`listBreakpoints`：4 类条件（on_phase / on_tool_call / on_iteration / on_tool_result）；断点命中时 `onBreakpoint` 事件 | 10 | 状态查询返回正确、断点命中触发事件 |
| S2.4 消息与上下文快照机制 | ✅ **已实现（裁剪版）**：裁减 `ConversationSnapshot`（rollback/editMessage 后续通过 HistoryManager 直接实现）；保留 `ContextSnapshot`（5 section 元数据：system_prompt / tool_definitions / skill_instructions / retrieved_memory / identity_context，含 size/token_estimate/hash）— 上下文级快照；主循环 `BuildContext` 阶段完成后自动创建上下文快照并推送 `onContextBuilt` 事件；`debugger.getContextSnapshot(iteration)`：返回指定轮次 5 section 元数据摘要（<500 字节/轮）；`debugger.getSection(iteration, section)`：懒加载 section 完整内容。<br>**实现期裁剪理由**：调试面板已明确不展示 conversation_history（左侧聊天面板已有），且 rollback/editMessage 可通过直接操作 HistoryManager 实现，无需独立快照存储层。 | 5 | 上下文快照元数据正确（5 section）、section 懒加载正确、onContextBuilt 事件推送 |

### Wave B：Gateway DevMode 集成 + Desktop 调试面板（2 周，5 项）

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|----------|
| S2.5 Gateway DevMode 启动参数 | 修改 `lifecycle/manager.rs`：Agent 标记 `dev: true` 时，启动参数追加 `--dev-mode`；`POST /api/agents/:id/start` 识别 `dev` 标记；新增 `GET /api/agents/:id/devmode` 查询 DevMode 状态和 Debug 端口 | 4 | DevMode Agent 以正确参数启动、端口可查询 |
| S2.6 Desktop 调试面板（基础 + 上下文树） | 前端实现：调试控制栏（Resume / Pause / Step / Stop 按钮）；当前状态显示（迭代、Phase、Token 用量）；连接 Debug Protocol WebSocket；开发者模式 toggle 切换；**上下文树视图**：按轮次展示 5 个控制面 section（system_prompt / tool_definitions / skill_instructions / retrieved_memory / identity_context），默认折叠，点展开后通过 `getSection` 懒加载内容；section hash 对比上一轮变更高亮；section 大小/token 估算显示；**不展示** conversation_history（左侧聊天面板已有） | 6 | 调试面板连接成功、控制命令生效、上下文树按轮次展开/折叠、section 懒加载正确、hash 对比高亮 |
| S2.7 断点面板 | 前端实现：断点列表（条件类型 + 参数 + 启用/禁用）；添加断点对话框（4 类条件）；删除断点；`onBreakpoint` 事件实时更新 | 4 | 断点 CRUD 操作正常、命中事件实时显示 |
| S2.8 Desktop 上下文编辑与回退 | 前端实现：消息右键菜单（Edit / Rollback to here）；上下文 section 内联编辑（点击 section 进入编辑模式，修改 system_prompt / tool_definitions / skill_instructions / retrieved_memory / identity_context）；`debugger.rewind({ to_iteration })`：回退到指定轮次起始状态并清空 patches；`debugger.patchContext({ patches })`：修补上下文 section（可多次调用增量构建）；`debugger.reExecute`：以修补后上下文重新执行当前轮次；回退/重执行后聊天面板自动刷新 | 6 | 上下文 section 编辑生效、rewind 回退正确（清除后续消息）、patchContext + reExecute 以修补后上下文生成新轮次 |
| S2.9 记忆调试面板（开发者模式） | **Debug Protocol 扩展**：<br>• `debugger.getMemoryState`：返回当前 Grafeo 状态（节点数、边数、各层分布）<br>• `debugger.getEpisodicFragments`：返回 Episodic 片段列表（含 decay_score 实时值）<br>• `debugger.triggerConsolidation`：手动触发离线巩固，返回合并结果报告<br>• `debugger.getConflictLog`：返回冲突检测日志（Negation/Evolution 事件）<br>• `debugger.triggerForgettingScan`：手动触发遗忘扫描，返回标记为 Dormant/Purged 的节点列表<br>**Desktop UI**：<br>• Episodic 片段浏览 + decay_score 实时显示（进度条/颜色编码）<br>• 巩固过程可视化（触发时机、合并结果、生成的新节点）<br>• 冲突检测日志查看（时间线形式，支持过滤）<br>• 手动触发遗忘扫描按钮 + 扫描结果预览 | 6 | 记忆调试命令正确响应、UI 实时展示 decay_score、巩固和遗忘扫描结果准确 |

> **前置依赖**：S2.9 依赖 S1.10 的 Gateway 记忆 API 已就绪；依赖 Grafeo 引擎的调试接口（`grafeo::debug` 模块）；巩固和遗忘扫描调用 `rollball-grafeo` 内部 API。

#### S2.10 Session Actor 边界场景测试（补充 Alex Review 建议）

| 编号 | 测试用例 | 预期 | 验收标准 |
|------|---------|------|----------|
| T51 | 多 Session 并发读写 JSONL | 两个 Session 同时写入各自 JSONL，Desktop App 读取不损坏 | JSONL 文件完整，无交错或损坏行 |
| T52 | Session 轮转原子性 | 模拟崩溃（步骤 1 后、步骤 2 后），恢复后状态正确 | 新文件存在 + 旧文件 ended，无错误状态 |
| T53 | Gateway IPC 断连降级 | Runtime 断开与 Gateway 的连接，5s 后重连 | 重连后 Session 继续运行，不丢消息 |
| T54 | Token 预算 per-session 隔离 | Session A 用完配额，Session B 仍可使用 | Session B 的 LLM 调用不受 Session A 配额影响 |
| T55 | Episode offset 去重 | 重复触发提炼，offset 正确递增 | 不产生重复 Episode |

> **前置依赖**：S2.0a AgentCore+SessionState 分离；依赖 conversation.rs 的 JSONL 操作和 episode_distill.rs 的 offset 逻辑。

**里程碑 M21：Debug Protocol 可用** — 连接 DevMode Agent → 暂停/步进 → 设断点 → 查看状态 → 编辑消息 → 记忆调试

---

## S3：开发框架高级能力（3~4 周，8 项任务）⏸️ **延后至 Phase 6**

Skill 热加载、Provider 动态切换、录制回放引擎。

> ⚠️ **延后决定（2026-05-09）**：S2 Debug Protocol 实现已与原始设计有较大偏离
> （Session Actor → 直接集成、watch channel → polling、ConversationSnapshot 裁剪等），
> S3 依赖的 `debugger.reloadSkills` / `debugger.switchProvider` / `debugger.startRecording`
> 等命令需要在新的 Debug Protocol 架构下重新设计。且 S3.3 Grafeo Skill 经验层是独立大型任务，
> S3.4~S3.7 录制回放引擎需求尚未明朗。
>
> **Phase 6 时将基于已有的 Debug Protocol 基础设施重新规划 S3。**
>
> **涉及 crate**：`rollball-runtime`（扩展 `skills/` + `debug/`）

### Wave A：Skill 热加载 + Provider 切换（1.5 周，3 项）

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|----------|
| S3.1 Skill 热加载 | 新增 `debugger.reloadSkills` 命令：重新扫描 `skills/` 目录；`SkillRegistry` 支持 `reload()` 方法；可选参数 `skill_name` 只重载指定 Skill；重载后通知 `onStateChange` 事件；Desktop Skill 编辑器：SKILL.md 编辑 + Reload to Runtime 按钮 | 8 | Skill 热加载不重启 Runtime、编辑器修改即时生效 |
| S3.2 Provider 动态切换 | 新增 `debugger.switchProvider` 命令：更新 LLM Client 当前 provider/model/base_url；需要新 Key 时通过 Gateway KeyRelease 获取；本地 Provider (ollama) 直连无需 Key；下次 LLM 调用使用新 provider；Desktop Provider 切换器 UI | 6 | 切换 provider 后 LLM 调用使用新配置 |
| S3.3 Grafeo Skill 经验层（核心数据结构） | 新增 `rollball-grafeo/src/skills/` 模块：`SkillDraft` / `SkillIteration` / `SkillExecution` / `SkillExperience` 节点类型；节点关系边（`HAS_ITERATION` / `EXECUTED_AS` / `PUBLISHED_AS` / `HAS_EXPERIENCE`）；CRUD 操作接口 | 8 | 经验层节点创建/查询/更新正确 |

### Wave B：录制回放引擎（1.5 周，5 项）

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|----------|
| S3.4 录制引擎 | 新增 `debug/recording.rs`：`debugger.startRecording` / `stopRecording`；录制数据：每步迭代记录 (type, content, iteration, usage)；JSONL 格式追加写入（崩溃不丢失）；保存到 Agent 工作区 `recordings/` 目录 | 6 | 录制文件生成、JSONL 格式正确 |
| S3.5 回放引擎 | 新增 `debug/replay.rs`：`debugger.loadRecording` / `stopReplay`；两种模式：auto（按录制顺序自动推进，可设延迟）+ manual（每步需 Step）；回放时注入录制步骤到主循环；`onRecordStep` 事件推送 | 8 | 自动/手动回放正确推进、事件推送正常 |
| S3.6 Desktop 录制回放 UI | 前端实现：录制控制栏（开始录制 / 停止录制 / 加载回放）；回放进度条（当前步骤 / 总步骤）；自动/手动模式切换；回放步骤详情展示 | 4 | 录制/回放操作通过 UI 完成 |
| S3.7 回放与编辑结合 | 支持回放过程中：编辑某步消息 + Re-execute；切换 Provider 后从某步重新执行；插入新用户消息偏离原路径；录制文件作为"回归测试用例" + "调试起点"双重用途 | 8 | 编辑/切换/插入操作在回放中生效 |
| S3.8 Skill 管理增强 | **Desktop UI**：<br>• Skill 列表 + 新建 Skill 入口（从模板或空白创建 SKILL.md）<br>• Skill 试运行功能：`POST /api/agents/:id/skills/:name/test`，传入测试输入，返回执行结果和日志<br>• Skill 版本/迭代历史浏览：对应 `SkillIteration` 节点查询和展示（迭代时间、变更摘要、发布状态）<br>• 经验层可视化：`SkillExperience` 节点查询和展示（触发场景、执行成功率、性能统计）<br>**Gateway HTTP API**（新增）：<br>• `POST /api/agents/:id/skills/:name/test` — Skill 试运行<br>• `GET /api/agents/:id/skills/:name/iterations` — Skill 迭代历史<br>• `GET /api/agents/:id/skills/:name/experiences` — Skill 经验节点查询 | 8 | Skill 试运行正确执行、迭代历史完整展示、经验层数据准确 |

> **前置依赖**：S3.8 依赖 S3.1 Skill 热加载（SkillRegistry 可查询）；依赖 S3.3 Grafeo Skill 经验层（`SkillIteration` / `SkillExperience` 节点已创建）；依赖 S1.11 Skill 浏览器（UI 基础组件复用）。

**里程碑 M22：开发框架可用** — Skill 热加载 → Provider 切换 → 录制 → 回放 → 编辑 → 重执行 → Skill 管理

---

## S4：发布工具链（2 周，7 项任务）

Agent 克隆、发布检查、打包签名、分发。

**涉及 crate**：`rollball-gateway`（新增克隆/发布 API）、`rollball-sign`（验证集成）

> **开发优先级调整**：S1 阶段（Desktop App 骨架）已完成，但缺乏可用 Agent 包进行功能验证。
> 先完成 S4 Wave A（后端 API），打通「examples → 打包 → 安装 → 对话测试」闭环，
> 再继续 Wave B（Desktop UI）。

### Wave A：Agent 克隆 + 发布 API（1 周，3 项）

#### S4.1 Agent 克隆 API

`POST /api/agents/:id/clone`

| 子任务 | 文件 | 验收标准 |
|--------|------|----------|
| S4.1.1 定义 CloneRequest/CloneResponse 结构体 | `http/publish_api.rs` | `CloneRequest { new_agent_id, mode: skeleton|full }`，`CloneResponse { agent_id, install_path }` 编译通过 |
| S4.1.2 clone_agent 核心逻辑 | `package_manager/clone.rs` | 骨架克隆：复制 manifest + prompts + config + tools + resources；完整克隆：额外复制 skills + data + **conversations/（当前 session JSONL 快照，支持"聊天到一半开启调试"场景）** + memory/private.grafeo；新 manifest 的 agent_id 替换为 new_agent_id；dev 字段设为 true；系统 Agent（system=true）不可克隆 |
| S4.1.3 注册克隆后的 Agent | `package_manager/clone.rs` | 调用 `state.add_installed()` 注册；Capability 自动注册（复用 add_installed 逻辑） |
| S4.1.4 HTTP route 注册 | `http/routes.rs`, `http/agents.rs` | `POST /api/agents/{id}/clone` 挂载到 router |
| S4.1.5 单元测试 | `package_manager/clone.rs`, `http/agents.rs` | 骨架克隆正确、完整克隆正确、dev 标记设置、系统 Agent 不可克隆、重复 agent_id 报错 |

预期测试数：6

#### S4.2 发布检查与清理 API

`POST /api/agents/:id/publish/prepare`

| 子任务 | 文件 | 验收标准 |
|--------|------|----------|
| S4.2.1 定义 PrepareRequest/PrepareResponse 结构体 | `http/publish_api.rs` | `PrepareRequest { clean: bool }`；`PrepareResponse { checks, warnings, errors, cleaned }` 编译通过 |
| S4.2.2 manifest 完整性校验 | `package_manager/publish.rs` | 必填字段（agent_id/version/name/description/author/runtime_version）非空；llm 配置完整（provider + model）；agent_id 格式校验（反向域名） |
| S4.2.3 prompts 存在性检查 | `package_manager/publish.rs` | 检查 install_path/prompts/system.md 是否存在；检查至少一个 prompt 文件 |
| S4.2.4 skills 格式校验（可选） | `package_manager/publish.rs` | 如果 skills/ 目录存在，检查每个 SKILL.md 是否有 YAML frontmatter |
| S4.2.5 清理操作 | `package_manager/publish.rs` | 移除 dev 标记（manifest.dev = false 并重写）；清空 recordings/ 目录；重置 config/settings.toml 为默认 |
| S4.2.6 HTTP route 注册 | `http/routes.rs` | `POST /api/agents/{id}/publish/prepare` 挂载到 router |
| S4.2.7 单元测试 | `package_manager/publish.rs` | 完整 manifest 通过检查；缺失字段返回错误；清理操作生效 |

预期测试数：8

#### S4.3 打包与签名 API

`POST /api/agents/:id/publish/build`

| 子任务 | 文件 | 验收标准 |
|--------|------|----------|
| S4.3.1 定义 BuildRequest/BuildResponse 结构体 | `http/publish_api.rs` | `BuildRequest { sign: bool, key_dir: Option<String> }`；`BuildResponse { output_path, signed, file_size }` 编译通过 |
| S4.3.2 build_package 核心打包逻辑 | `package_manager/publish.rs` | 读取 install_path 下所有文件；按 .agent ZIP 格式打包（manifest.toml + prompts/ + config/ + data/ + skills/ + tools/ + resources/）；输出到 `build/<agent_id>-<version>.agent`；ZIP 内不包含 META-INF/SIGNING.BLOCK |
| S4.3.3 可选签名集成 | `package_manager/publish.rs` | sign=true 时调用 `rollball_sign::sign::sign_package()`；sign=false 时仅打包不签名；签名使用 dev key 或指定 key_dir |
| S4.3.4 install-locally API | `http/publish_api.rs` | `POST /api/agents/{id}/publish/install-locally`：复用 `install::install_package()` 安装 build 产物；自动覆盖已安装的同 agent_id（调用 upgrade 逻辑） |
| S4.3.5 export API | `http/publish_api.rs` | `POST /api/agents/{id}/publish/export`：返回 .agent 文件内容（binary download）；支持指定输出路径 |
| S4.3.6 HTTP route 注册 | `http/routes.rs` | 三个 publish 路由挂载到 router |
| S4.3.7 单元测试 | `package_manager/publish.rs` | 打包 ZIP 结构正确；签名包验证通过；未签名包 dev_mode 可安装；install-locally 成功；export 文件完整 |

预期测试数：8

#### S4.3a CLI 打包命令

`rollball-gateway package --source <dir> --output <dir> [--sign] [--key-dir <dir>]`

| 子任务 | 文件 | 验收标准 |
|--------|------|----------|
| S4.3a.1 新增 CLI Package 子命令 | `cli.rs` | `Commands::Package { source, output, sign, key_dir }` 解析正确 |
| S4.3a.2 package_agent CLI 逻辑 | `cli.rs` | 读取 source 目录下的 manifest.toml；调用 build_package 核心逻辑打包；输出 .agent 文件到 output 目录 |
| S4.3a.3 单元测试 | `cli.rs` | CLI 解析正确；打包 examples/weather-agent 成功 |

预期测试数：3

**为什么需要 CLI 打包**：当前 examples 目录下的 agent（system-agent、weather-agent）是目录格式（manifest.toml + prompts/），需要先打包成 .agent ZIP 格式才能通过 Gateway install 命令安装。CLI 打包命令不依赖 Desktop App，可在终端直接执行。

### Wave B：Desktop 发布向导 + 创建向导（1 周，3 项）

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|----------|
| S4.4 Desktop 发布向导 | 前端实现：5 步流程（检查 → 清理 → 打包 → 签名 → 分发）；检查结果展示（警告 + 错误）；签名密钥选择（已有/新生成）；分发选项（本地安装/导出/上传） | 4 | 发布向导端到端走通 |
| S4.5 Desktop Agent 克隆对话框 | 前端实现：克隆模式选择（骨架/完整）；新 Agent ID 输入；克隆结果展示；克隆后自动切换到新 Agent | 2 | 克隆操作通过 UI 完成 |
| S4.6 Desktop Agent 创建向导 | 前端实现：5 步流程（基本信息 → LLM 配置 → 权限声明 → 选择模板 → 生成）；模板选择（空白/天气/日历/自定义）；创建后自动进入 DevMode | 2 | 创建向导端到端走通 |

**里程碑 M23：发布工具链可用** — 克隆 Agent → 开发调试 → 发布检查 → 打包签名 → 分发

### 测试闭环验证

Wave A 完成后，执行以下端到端验证，确认 S1 Desktop App 可与 Agent 对话：

```bash
# 1. CLI 打包 system-agent
rollball package --source examples/system-agent --output build/

# 2. CLI 打包 weather-agent
rollball package --source examples/weather-agent --output build/

# 3. 通过 Gateway HTTP API 安装
curl -X POST http://127.0.0.1:19876/api/agents/install \
  -H 'Content-Type: application/json' \
  -d '{"package_path": "build/com.rollball.system-1.0.0.agent", "dev_mode": true}'

curl -X POST http://127.0.0.1:19876/api/agents/install \
  -H 'Content-Type: application/json' \
  -d '{"package_path": "build/com.example.weather-1.0.0.agent", "dev_mode": true}'

# 4. 启动 weather-agent
curl -X POST http://127.0.0.1:19876/api/agents/com.example.weather/start

# 5. 通过 Desktop App 对话测试
cargo tauri dev  # 启动 Desktop App，选择 weather-agent 对话
```

---

## S5：Phase 4 遗留技术债务 + 集成验证（3~4 周，10 项任务）

清偿从 Phase 4 延后的 13 项 P2 技术债务，并进行 Phase 5 全系统集成验证。

**涉及 crate**：`rollball-gateway`、`rollball-core`、`rollball-grafeo`、`rollball-runtime`

### Wave A：Gateway HTTP 健壮性债务（1 周，4 项）

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|----------|
| S5.1 API 错误响应格式统一 | P2-3：定义统一错误响应格式 `{"error": {"code": "...", "message": "...", "details": ...}}`；所有 HTTP API 错误使用统一格式；前端统一错误处理 | 4 | 所有 API 错误返回统一格式 |
| S5.2 HTTP API 请求限流 | P2-4：基于 `tower::ServiceBuilder` 添加 rate limiting 中间件；按 IP 限流（默认 60 req/min）；可配置限流参数；429 响应包含 `Retry-After` 头 | 4 | 超限请求返回 429、正常请求不受影响 |
| S5.3 API 版本控制 `/api/v1/` | P2-5：所有路由添加 `/api/v1/` 前缀；保留 `/api/` 作为兼容别名（当前版本）；版本协商机制（`Accept-Version` 头） | 4 | `/api/v1/` 路由正常工作、旧路径兼容 |
| S5.4 P2-5g MEM-04 遗忘机制冲突解决 | 讨论 PRD vs 代码实现差异；决定：更新 PRD（承认后台扫描）或修改代码（改为按需计算）；执行决策 + 更新文档 | 2 | 冲突解决、文档一致 |

### Wave B：Permission + Cron 债务（1 周，4 项）

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|----------|
| S5.5 PermissionGrant 序列化压缩 | P2-8：序列化时使用二进制格式（bincode 或 msgpack）；减少 IPC 传输开销；向后兼容 JSON 格式 | 3 | 序列化体积减小、反序列化正确 |
| S5.6 PermissionPolicy 运行时可配置 | P2-9：从配置文件加载 PermissionPolicy（auto-approve/auto-deny 规则）；支持热重载；Default policy 可覆盖 | 3 | 配置加载生效、热重载不重启 |
| S5.7 PermissionChecker 监控指标 | P2-10：记录缓存命中率、请求延迟、按权限类型统计；暴露 `/api/status/permissions` 端点 | 3 | 指标可查询、缓存命中率统计正确 |
| S5.8 Cron 增强 | P2-11~14：时区支持（UTC/本地/指定）；重试机制（可配置次数+间隔）；批量操作 API（batch add/delete）；最大执行次数（`max_runs` / `expires_at`） | 6 | 时区正确、重试生效、批量操作、执行次数限制 |

### Wave C：Grafeo 债务 + 集成验证（1~2 周，2 项）

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|----------|
| S5.9 Grafeo 优化 | P2-3g：冲突检测 Negation/Evolution keywords 可配置（从硬编码改为配置文件）；P2-4g：PageRank 增量优化（大图场景下避免 O(V²) 全量计算，采用采样策略或增量更新） | 6 | keywords 可配置、PageRank 大图性能改善 |
| S5.10 Phase 5 全链路集成验证 | 端到端场景：Desktop App 安装 Agent → 对话 → 切换 DevMode → 步进调试 → 设断点 → Skill 编辑热加载 → 录制 → 回放 → 克隆 → 发布 → API 版本控制 → 限流；性能基准测试（HTTP P99、调试延迟、录制回放吞吐） | 12 | 全链路通过、性能指标可量化 |

**里程碑 M24：技术债务清零** — P2 问题全部解决、集成验证通过

**里程碑 M25：Phase 5 交付** — Desktop App + Debug Protocol + 开发框架 + 发布工具链 + 技术债务清零

---

## 依赖关系

```
S1（Desktop App）──┬──→ S2（Debug Protocol）
                   │
                   └──→ S4（发布工具链）──→ S5（技术债务 + 集成验证）

                   ┌── S3 延后至 Phase 6
```

- S1 和 S4 可部分并行（S4 Gateway API 不依赖 Desktop UI）
- S2 依赖 S1 的 Desktop App 骨架（调试面板需要 UI）
- ~~S3 依赖 S2 的 Debug Protocol 基础~~ → S3 整体延后至 Phase 6
- S5 依赖 S1~S4 全部完成

---

## 技术栈新增

| 组件 | 选择 | 版本 | 用途 |
|------|------|------|------|
| Tauri | v2 | 2.x | Desktop App 框架 |
| React | 19 | 19.x | 前端 UI |
| TypeScript | 5 | 5.x | 前端类型安全 |
| Vite | 6 | 6.x | 构建工具 + HMR |
| Tailwind CSS | 4 | 4.x | 样式系统 |
| shadcn/ui | latest | - | UI 组件库 |
| Zustand | 5 | 5.x | 状态管理 |
| tokio-tungstenite | 0.26 | 0.26 | Debug Protocol WebSocket |

---

## 风险与缓解

| 风险 | 影响 | 缓解措施 |
|------|------|---------|
| Tauri v2 API 不稳定 | Desktop App 开发受阻 | 锁定 Tauri v2 LTS 版本，跟随 minor 更新 |
| Debug Protocol 延迟影响主循环 | DevMode 性能问题 | 生产模式零开销（`cfg(dev_mode)` 条件编译） |
| 跨平台 Webview 差异 | UI 渲染不一致 | 优先 Linux 验证，CI 加入 macOS 测试 |
| Grafeo 经验层设计复杂 | S3.3 开发周期超预期 | 先实现 SkillDraft + SkillExecution，SkillExperience 迭代 |
| PageRank O(V²) 优化困难 | P2-4g 可能延后 | 采样策略降级方案（top-k 采样代替全量计算） |

---

## 延后至 Phase 6+ 的项目

以下内容不在 Phase 5 范围内：

| 内容 | 原因 | 目标阶段 |
|------|------|---------|
| 消息编辑 + 重执行的 LLM 回滚保证 | 需要 Agent Runtime 对消息变更的完整回滚语义 | Phase 6 |
| 对比回放（多录制文件同屏对比） | UI 复杂度高，核心回放功能优先 | Phase 6 |
| Skill 级联降级 | 设计问题待积累经验后决策 | Phase 6 |
| Manifest 编辑器 | 非核心，文本编辑器即可满足 | Phase 6 |
| 移动端适配 | 需 Phase 7 跨平台基础设施 | Phase 7 |
