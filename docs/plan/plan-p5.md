# Phase 5: Desktop App + 开发框架 — 实施计划

> 版本：v1.0 | 更新日期：2026-04-27
> 前置条件：Phase 4（M15~M19）全部完成
> 预计周期：16~18 周
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

| 阶段 | 主题 | 任务数 | 预期测试 | 预计周期 |
|------|------|--------|---------|---------|
| S1 | Desktop App 骨架 + 系统托盘 | 9 | 45 | 4 周 |
| S2 | Debug Protocol 实现 | 8 | 52 | 4 周 |
| S3 | 开发框架高级能力 | 7 | 40 | 3 周 |
| S4 | 发布工具链 | 6 | 30 | 2 周 |
| S5 | Phase 4 遗留技术债务 + 集成验证 | 10 | 50 | 3~4 周 |
| **合计** | | **40** | **217** | **16~18 周** |

---

## S1：Desktop App 骨架 + 系统托盘（4 周，9 项任务）

Desktop App 基础设施搭建，用户模式下的完整功能闭环。

**涉及 crate**：新增 `apps/rollball-desktop/`（Tauri 项目）

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
| S1.9 执行结果区（用户模式） | 前端实现：工具调用摘要（工具名、参数、耗时、状态）；Token 用量统计（prompt/completion/total）；当前 Agent 运行状态 | 3 | 结果区正确展示工具调用和 Token 用量 |

**里程碑 M20：Desktop App 用户模式可用** — 安装 Agent → 对话 → 查看结果 → 设置管理

---

## S2：Debug Protocol 实现（4 周，8 项任务）

Agent Runtime DevMode 的完整实现，包含 Debug Protocol Server、执行控制、状态查询、快照机制。

**涉及 crate**：`rollball-runtime`（新增 `debug/` 模块）、`rollball-gateway`（新增 DevMode 启动参数）

### Wave A：Debug Protocol Server + 执行控制（2 周，4 项）

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|----------|
| S2.1 Debug Protocol Server | 新增 `rollball-runtime/src/debug/server.rs`：WebSocket 服务端（`tokio-tungstenite`，`ws://127.0.0.1:19877`）；JSON-RPC 2.0 消息解析/响应；DevMode 检测（`--dev-mode` 启动时才监听）；连接管理（单客户端，后续连接拒绝） | 8 | WebSocket 连接建立/断开、JSON-RPC 消息收发 |
| S2.2 执行控制（Pause/Step/Resume/Stop） | 修改主循环 `loop_.rs`：引入 `DebugController`（`Arc<Mutex<DebugState>>`）；Pause 时主循环 `tokio::sync::watch` 阻塞等待 Resume；Step 执行一步后自动 Pause；Stop 终止当前对话；`onStep` 事件推送 | 10 | 执行控制命令正确响应、主循环暂停/恢复 |
| S2.3 状态查询 + 断点 | `debugger.getState`：返回当前迭代、Phase、消息列表、快照 IDs、断点、Token 用量；`debugger.setBreakpoint`/`removeBreakpoint`/`listBreakpoints`：4 类条件（on_phase / on_tool_call / on_iteration / on_tool_result）；断点命中时 `onBreakpoint` 事件 | 10 | 状态查询返回正确、断点命中触发事件 |
| S2.4 消息快照机制 | 新增 `debug/snapshot.rs`：`ConversationSnapshot`（iteration, message_count, cumulative_usage, timestamp）；主循环每步迭代结束自动创建快照；`debugger.rollback(target_index)`：截断 messages 到目标长度；`debugger.editMessage(index, content)`：修改指定消息 | 8 | 快照创建、回滚截断正确、消息编辑生效 |

### Wave B：Gateway DevMode 集成 + Desktop 调试面板（2 周，4 项）

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|----------|
| S2.5 Gateway DevMode 启动参数 | 修改 `lifecycle/manager.rs`：Agent 标记 `dev: true` 时，启动参数追加 `--dev-mode`；`POST /api/agents/:id/start` 识别 `dev` 标记；新增 `GET /api/agents/:id/devmode` 查询 DevMode 状态和 Debug 端口 | 4 | DevMode Agent 以正确参数启动、端口可查询 |
| S2.6 Desktop 调试面板（基础） | 前端实现：调试控制栏（Resume / Pause / Step / Stop 按钮）；当前状态显示（迭代、Phase、Token 用量）；连接 Debug Protocol WebSocket；开发者模式 toggle 切换 | 4 | 调试面板连接成功、控制命令生效 |
| S2.7 断点面板 | 前端实现：断点列表（条件类型 + 参数 + 启用/禁用）；添加断点对话框（4 类条件）；删除断点；`onBreakpoint` 事件实时更新 | 4 | 断点 CRUD 操作正常、命中事件实时显示 |
| S2.8 Desktop 消息编辑与回滚 | 前端实现：消息右键菜单（Edit / Rollback to here / Re-execute from here）；编辑模式（消息内容可编辑文本框）；`debugger.editMessage` / `debugger.rollback` / `debugger.reExecute` 调用 | 4 | 消息编辑、回滚、重执行功能正常 |

**里程碑 M21：Debug Protocol 可用** — 连接 DevMode Agent → 暂停/步进 → 设断点 → 查看状态 → 编辑消息

---

## S3：开发框架高级能力（3 周，7 项任务）

Skill 热加载、Provider 动态切换、录制回放引擎。

**涉及 crate**：`rollball-runtime`（扩展 `skills/` + `debug/`）

### Wave A：Skill 热加载 + Provider 切换（1.5 周，3 项）

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|----------|
| S3.1 Skill 热加载 | 新增 `debugger.reloadSkills` 命令：重新扫描 `skills/` 目录；`SkillRegistry` 支持 `reload()` 方法；可选参数 `skill_name` 只重载指定 Skill；重载后通知 `onStateChange` 事件；Desktop Skill 编辑器：SKILL.md 编辑 + Reload to Runtime 按钮 | 8 | Skill 热加载不重启 Runtime、编辑器修改即时生效 |
| S3.2 Provider 动态切换 | 新增 `debugger.switchProvider` 命令：更新 LLM Client 当前 provider/model/base_url；需要新 Key 时通过 Gateway KeyRelease 获取；本地 Provider (ollama) 直连无需 Key；下次 LLM 调用使用新 provider；Desktop Provider 切换器 UI | 6 | 切换 provider 后 LLM 调用使用新配置 |
| S3.3 Grafeo Skill 经验层（核心数据结构） | 新增 `rollball-grafeo/src/skills/` 模块：`SkillDraft` / `SkillIteration` / `SkillExecution` / `SkillExperience` 节点类型；节点关系边（`HAS_ITERATION` / `EXECUTED_AS` / `PUBLISHED_AS` / `HAS_EXPERIENCE`）；CRUD 操作接口 | 8 | 经验层节点创建/查询/更新正确 |

### Wave B：录制回放引擎（1.5 周，4 项）

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|----------|
| S3.4 录制引擎 | 新增 `debug/recording.rs`：`debugger.startRecording` / `stopRecording`；录制数据：每步迭代记录 (type, content, iteration, usage)；JSONL 格式追加写入（崩溃不丢失）；保存到 Agent 工作区 `recordings/` 目录 | 6 | 录制文件生成、JSONL 格式正确 |
| S3.5 回放引擎 | 新增 `debug/replay.rs`：`debugger.loadRecording` / `stopReplay`；两种模式：auto（按录制顺序自动推进，可设延迟）+ manual（每步需 Step）；回放时注入录制步骤到主循环；`onRecordStep` 事件推送 | 8 | 自动/手动回放正确推进、事件推送正常 |
| S3.6 Desktop 录制回放 UI | 前端实现：录制控制栏（开始录制 / 停止录制 / 加载回放）；回放进度条（当前步骤 / 总步骤）；自动/手动模式切换；回放步骤详情展示 | 4 | 录制/回放操作通过 UI 完成 |
| S3.7 回放与编辑结合 | 支持回放过程中：编辑某步消息 + Re-execute；切换 Provider 后从某步重新执行；插入新用户消息偏离原路径；录制文件作为"回归测试用例" + "调试起点"双重用途 | 8 | 编辑/切换/插入操作在回放中生效 |

**里程碑 M22：开发框架可用** — Skill 热加载 → Provider 切换 → 录制 → 回放 → 编辑 → 重执行

---

## S4：发布工具链（2 周，6 项任务）

Agent 克隆、发布检查、打包签名、分发。

**涉及 crate**：`rollball-gateway`（新增克隆/发布 API）、`rollball-sign`（验证集成）

### Wave A：Agent 克隆 + 发布 API（1 周，3 项）

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|----------|
| S4.1 Agent 克隆 API | Gateway 新增 `POST /api/agents/:id/clone`：骨架克隆（manifest + prompts + config + tools + resources）；完整克隆（+ skills + data + Grafeo 快照）；新 Agent 标记 `dev: true`；系统 Agent 不可克隆 | 6 | 骨架/完整克隆正确、dev 标记设置 |
| S4.2 发布检查与清理 API | Gateway 新增 `POST /api/agents/:id/publish/prepare`：manifest 完整性校验、SKILL.md 格式校验、prompts 存在性检查；清理操作：移除 dev 标记、清空 recordings/、清空私有 Grafeo、重置 config | 8 | 检查发现的问题正确返回、清理操作执行成功 |
| S4.3 打包与签名 API | Gateway 新增 `POST /api/agents/:id/publish/build`：按 .agent 包格式打包为 ZIP；可选签名（调用 rollball-sign）；输出到 `build/<agent_id>-<version>.agent`；新增 `POST /api/agents/:id/publish/install-locally` 和 `POST /api/agents/:id/publish/export` | 8 | 打包 ZIP 正确、签名验证通过、本地安装成功 |

### Wave B：Desktop 发布向导 + 创建向导（1 周，3 项）

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|----------|
| S4.4 Desktop 发布向导 | 前端实现：5 步流程（检查 → 清理 → 打包 → 签名 → 分发）；检查结果展示（警告 + 错误）；签名密钥选择（已有/新生成）；分发选项（本地安装/导出/上传） | 4 | 发布向导端到端走通 |
| S4.5 Desktop Agent 克隆对话框 | 前端实现：克隆模式选择（骨架/完整）；新 Agent ID 输入；克隆结果展示；克隆后自动切换到新 Agent | 2 | 克隆操作通过 UI 完成 |
| S4.6 Desktop Agent 创建向导 | 前端实现：5 步流程（基本信息 → LLM 配置 → 权限声明 → 选择模板 → 生成）；模板选择（空白/天气/日历/自定义）；创建后自动进入 DevMode | 2 | 创建向导端到端走通 |

**里程碑 M23：发布工具链可用** — 克隆 Agent → 开发调试 → 发布检查 → 打包签名 → 分发

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
S1（Desktop App）──┬──→ S2（Debug Protocol）──→ S3（开发框架高级）
                   │                                │
                   └──→ S4（发布工具链）──────────────┤
                                                    ↓
                                              S5（技术债务 + 集成验证）
```

- S1 和 S4 可部分并行（S4 Gateway API 不依赖 Desktop UI）
- S2 依赖 S1 的 Desktop App 骨架（调试面板需要 UI）
- S3 依赖 S2 的 Debug Protocol 基础（热加载/录制走 Debug 命令）
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
