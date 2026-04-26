# Phase 4: HTTP API 与通信完善 — 实施计划

> 版本：v1.0 | 更新日期：2026-04-26
> 前置条件：Phase 3（M10~M14）全部完成
> 预计周期：12~14 周
> 预计里程碑：M15~M19

---

## 背景与目标

### 已完成阶段回顾

| Phase | 主题 | 里程碑 | 状态 |
|-------|------|--------|------|
| Phase 1 | 基础框架 + LLM 交互 | M1~M4 | ✅ 完成 |
| Phase 2 | Grafeo 记忆 + System Agent + Intent + 多 Provider | M5~M9 | ✅ 完成 |
| Phase 3 | 权限框架 + WASM 沙箱 + Shell 安全 + 离线巩固 | M10~M14 | ✅ 完成（94% PRD 一致性，179/210 测试）|

### Phase 3 遗留问题

基于 `docs/review/04-p3-code-review.md` 的审查结果：

| 编号 | 严重度 | 问题描述 | 处理方式 |
|------|--------|----------|----------|
| P0-3 | P0 | PermissionRequest IPC 消息协议未定义（S1.5 延期） | S2 优先解决 |
| P1-1 | P1 | Approval Gate CLI 未真正阻塞（自动批准 Medium/High） | S5 修复 |
| P2-1 | P2 | WASM Store 缺少 ResourceLimiter（恶意模块可声明无限内存） | S5 修复 |
| P2-2 | P2 | 三元组去重 `is_duplicate()` 命名误导（实际检测潜在冲突） | S5 修复 |
| P2-3 | P2 | FsWatcher 使用同步 `std::sync::mpsc`（可能阻塞 async Runtime） | S5 修复 |
| P1-2 | P1 | 权限字符串解析无错误信息（返回 None 而非 Result） | S5 修复 |

### Roadmap Phase 4 "通信与协调" 完成度

原 Roadmap Phase 4 定义的 4 项能力，3 项已在 Phase 2 交付：

| 能力 | 状态 | 交付阶段 |
|------|------|----------|
| Intent 跨 Agent 消息转发 + Capability Registry | ✅ 完成 | Phase 2 S4 |
| Budget Tracker（用量上报 + 超限信号） | ✅ 完成 | Phase 2 S4 |
| Rate Limiter（速率令牌分配） | ✅ 完成 | Phase 2 S4 |
| 定时触发器（cron 解析） | 🚧 模块已有 | Cron 模块 16.6KB，需集成 |

### Phase 4 的核心价值

Phase 4 的核心定位是**通信基础设施完善**——补齐 Desktop App 的硬依赖（HTTP API），解决 Phase 3 遗留的安全缺口（Permission IPC），完成 Roadmap Phase 4 遗留项（Cron），并扩展企业级检索能力（RAG 工具）。

1. **HTTP API**：Desktop App 的唯一接入层，当前 Gateway 仅有 Socket API（二进制帧，面向 Agent Runtime），无 HTTP/REST 接口
2. **Permission IPC**：Phase 3 权限框架的运行时请求链路断裂——Runtime 无法向 Gateway 发起权限请求，Approval Gate 无真正用户交互
3. **Cron 触发器**：CronScheduler 模块已实现，但未与 Gateway 事件循环和 Agent 生命周期集成
4. **RAG 工具**：企业级 Agent 的核心差异化能力，实现双通道检索模型（Grafeo + 企业 RAG）

---

## 阶段划分

### S1：Gateway HTTP API（4 周，9 项任务）

实现 `docs/04-gateway.md` §9 定义的完整 HTTP API，作为 Desktop App 和 CLI 的统一接入层。这是 Phase 5（Desktop App）的硬前置条件。

**涉及 crate**：`rollball-gateway`（新增 `http/` 模块）

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|---------|
| S1.1 Axum HTTP Server 框架 | 引入 axum/tower crate；`HttpServer` 结构体；与 Gateway 共享 `Arc<RwLock<GatewayState>>`；`HttpConfig`（host/port/cors）配置解析；Gateway `run()` 中并行启动 HTTP + IPC | 6 | Server 启动/关闭、配置解析、与 IPC 共享状态 |
| S1.2 Agent 管理 API | `GET /api/agents`（列表+状态）、`GET /api/agents/:id`（详情）、`POST /api/agents/install`（安装含签名验证）、`DELETE /api/agents/:id`（卸载）、`POST /api/agents/:id/start`、`POST /api/agents/:id/stop` | 12 | 安装/卸载/启停全链路、签名无效拒绝安装 |
| S1.3 对话 API | `POST /api/agents/:id/message`（发送消息）、`GET /api/agents/:id/stream`（WebSocket 升级，流式输出 chunk/tool_call/tool_result/done） | 10 | 消息发送、WebSocket 握手、流式推送 |
| S1.4 Vault API | `GET /api/vault/keys`（脱敏列表，key_preview 前后3字符）、`POST /api/vault/keys`（添加 Key）、`DELETE /api/vault/keys/:provider`（删除）、`PUT /api/vault/keys/:provider`（更新） | 8 | Key 不明文返回、CRUD 完整 |
| S1.5 配置与状态 API | `GET /api/config`（Gateway 配置）、`PUT /api/config`（更新配置）、`GET /api/status`（系统状态：版本/运行数/内存）、`GET /health`（健康检查） | 6 | 状态正确、配置热更新 |
| S1.6 消息转发桥接 | HTTP 对话请求 → Gateway 内部 → Socket 转发给 Agent Runtime → Runtime 响应 → Gateway → HTTP WebSocket 推送；复用 `SessionManager` 的 server-push 通道 | 8 | HTTP → Socket → HTTP 全链路消息可达 |
| S1.7 安全与认证 | 仅监听 127.0.0.1；可选 Auth Token（Gateway 生成随机 token 写入 `http_token` 文件）；Desktop App 首次连接时读取 token | 6 | 非 localhost 拒绝、token 校验 |
| S1.8 Desktop App 发现机制 | pidfile 发现策略（`gateway.pid` 含 pid/http_port/socket_path）；端口冲突自动递增（19876→19877→19878）；Desktop App 优先级：自身配置 → pidfile → 默认地址 → 手动配置 | 4 | 发现正确、端口递增 |
| S1.9 HTTP API 集成测试 | Axum test 框架（`tower::ServiceExt`）；Agent 安装→启动→对话→停止 全链路；Vault CRUD；流式 WebSocket | 10 | 全 API 端到端通过 |

**里程碑 M15：HTTP API 可用** — Desktop App / CLI 可通过 HTTP 管理和对话

预期测试合计：70 项

---

### S2：Permission IPC 协议与 Intent 权限校验（2 周，6 项任务）

解决 Phase 3 审查 P0-3（PermissionRequest IPC 消息设计缺失），补全权限框架的运行时请求链路，并将权限校验应用到 Intent 路由。

**涉及 crate**：`rollball-core`、`rollball-gateway`、`rollball-runtime`

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|---------|
| S2.1 PermissionRequest/Response 消息协议 | 在 `GatewayRequest`/`GatewayResponse` 中增加 `PermissionRequest`/`PermissionResult` 消息类型（协议层已在 06-communication.md §1.4 定义但未实现）；定义超时策略（Runtime 侧默认 60s 超时后拒绝工具调用） | 6 | 消息序列化/反序列化、超时逻辑 |
| S2.2 Gateway 侧 Permission Request 处理 | IPC Server 收到 PermissionRequest 后不阻塞主循环（独立 tokio task 处理）；查询 PermissionStore 已授权 → 有则直接批准；无已授权记录 → CLI 模式输出请求（预留 Desktop App 弹窗接口）；用户响应后更新 PermissionStore 并回传 | 8 | 已授权自动批准、未授权走请求流程、不阻塞 IPC |
| S2.3 Runtime 侧 Permission 请求发送 | PermissionChecker 缓存未命中时，通过 IPC Client 发送 PermissionRequest；等待响应或超时（60s）；超时后工具调用返回 PermissionDenied | 6 | 缓存命中直接执行、未命中发请求、超时拒绝 |
| S2.4 Intent 权限校验 | Gateway Intent Router 在路由前校验：发送方是否持有 `intent:send` 权限；目标 Agent 的 capability 是否匹配请求的 action；params 大小限制 64KB | 8 | 无 intent:send 权限拒绝、capability 不匹配拒绝、超限拒绝 |
| S2.5 Permission HTTP API | `GET /api/agents/:id/permissions`（查询授权列表）、`POST /api/agents/:id/permissions/:perm/grant`（授权）、`DELETE /api/agents/:id/permissions/:perm`（撤销） | 6 | HTTP 接口可查询/授权/撤销 |
| S2.6 Permission IPC 集成测试 | 端到端：Runtime 权限缓存 miss → IPC 请求 → Gateway 查询/交互 → 响应 → Runtime 执行工具 | 4 | 全链路权限请求/授权/执行 |

**里程碑 M16：权限 IPC 与 Intent 权限校验可用** — 运行时权限请求全链路打通

预期测试合计：38 项

---

### S3：Cron 触发器集成（2 周，5 项任务）

CronScheduler 模块已实现（`rollball-gateway/src/cron/mod.rs`，16.6KB，含 5 字段 cron 表达式解析），但未与 Gateway 事件循环、Agent 生命周期、IPC 消息集成。S3 完成完整集成。

**涉及 crate**：`rollball-gateway`

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|---------|
| S3.1 Cron 触发器与 Gateway 事件循环集成 | Gateway `run()` 中启动 CronScheduler tick 循环；触发时查找目标 Agent：已运行 → 通过 SessionManager server-push 推送 IntentReceived；未运行 → 拉起 Agent 进程（复用 LifecycleManager）再推送 | 8 | 触发后 Agent 收到 Intent、未运行 Agent 被拉起 |
| S3.2 Cron 条目持久化 | CronEntry 存储到 rusqlite（Gateway data 目录 `cron_entries.db`）；Gateway 重启后自动恢复所有 cron 条目；Schema 版本管理 | 6 | 持久化/恢复/版本管理 |
| S3.3 Manifest Trigger 配置解析 | `rollball-core/src/manifest.rs` 已有 `Trigger` 结构和 `triggers` 字段；完善 cron 类型 trigger 解析（schedule + action + params）；安装时注册 cron 条目，卸载时清理 | 6 | manifest cron 触发器注册/清理 |
| S3.4 Cron 管理 API | Socket API：`CronRegister`/`CronUnregister`/`CronList` 请求类型；HTTP API：`POST /api/agents/:id/cron`（注册）、`DELETE /api/agents/:id/cron/:cron_id`（取消）、`GET /api/agents/:id/cron`（列表）；CLI：`rollball cron list`、`rollball cron add`、`rollball cron remove` | 8 | Socket/HTTP/CLI 三入口管理 |
| S3.5 Cron 集成测试 | 端到端：manifest 声明 cron → 安装时注册 → Gateway 触发 → Agent 收到 Intent → 查询/取消 cron | 6 | 定时触发 + 持久化恢复 |

**里程碑 M17：Cron 触发器可用** — 定时触发全链路打通

预期测试合计：34 项

---

### S4：RAG 工具集成（3 周，8 项任务）

实现 `docs/00-prd.md` §1.13 定义的企业 RAG 集成能力。RAG 是独立检索通道，不整合进 Memory 抽象层，通过标准 `rag` 工具暴露给 LLM。

#### RAG 设计原则（配置驱动，Opt-In）

**核心约束：RAG 不是默认能力，而是特定 Agent 通过 manifest 声明 opt-in 的扩展能力。** 未声明 RAG 的 Agent，Runtime 行为与 Phase 3 完全一致，零侵入。

RAG 配置驱动的 Runtime 行为差异：

| Runtime 行为 | manifest 无 RAG 声明 | manifest 有 RAG 声明 |
|-------------|--------------------|--------------------|
| 步骤② MemoryManager.retrieve() | 仅查 Grafeo 通道 | 并行查 Grafeo + RAG 双通道 |
| 步骤② 上下文注入 | 仅 Grafeo 检索结果 | Grafeo + RAG 结果拼接，按来源标注 |
| 步骤③ LLM Tool Definitions | 不含 RAG 工具 | 含 RAG 工具（可显式调用） |
| 步骤⑤ Tool Dispatch | 无 RAG 工具路由 | RAG 工具 → RagClient HTTP 调用 |
| PermissionChecker | 无 rag 相关权限 | 校验 `rag:query` + `network:<rag_url>` |
| Vault | 不请求 RAG Key | 启动时通过 Socket 获取 RAG 认证凭据 |

#### RAG 触发模型：混合双触发（ADR-012）

RAG 有两种触发方式，均由 manifest 配置使能：

**触发 1：自动检索（MemoryManager Retrieve 阶段，步骤②）**

每轮迭代自动触发，用当前用户消息作 query，轻量查询（top_k=3，score_threshold=0.7）：

```
步骤② MemoryManager.retrieve()
  ├─ Grafeo 通道: hybrid_search + graph_expand  ← 始终执行
  └─ RAG 通道: RagClient.query(用户消息, top_k=3)  ← 仅 manifest 声明 RAG 时执行
     ├─ 成功 → 结果按来源标注 [Grafeo] / [RAG:enterprise_knowledge]
     ├─ 超时(5s) → 跳过 RAG 通道，仅用 Grafeo 结果
     └─ 不可达 → 同上，不阻塞 Agent
  结果合并、去重、按 token 预算裁剪后注入 LLM 上下文
```

**触发 2：显式工具调用（Tool Dispatch 阶段，步骤⑤）**

LLM 主动调用 RAG 工具，用于针对性深入查询（不同 query、filter、更高 top_k）：

```
步骤⑤ Tool Dispatch
  └─ LLM 输出 tool_call: enterprise_knowledge(query="Q3产品路线图", top_k=10)
     ├─ Permission Check: rag:query + network:<endpoint_url>
     ├─ 从 Vault 获取认证凭据
     ├─ RagClient.query(query, top_k=10, filters=...)
     ├─ 结果标注 source_url / chunk_id
     └─ 返回 tool result 给 LLM
```

**去重策略**：自动通道结果作为"背景上下文"注入，显式工具结果作为"工具返回值"追加到 History，两者在上下文中位置不同，语义不重叠。

#### Rollball 不实现 RAG，只定义标准查询协议

Rollball 定义标准 HTTP 查询协议（请求/响应格式），企业 RAG 自行适配此协议：

- 请求：`POST <endpoint>`，body: `{ protocol_version: "1.0", query, collection?, top_k, score_threshold?, filters? }`
- 响应：`{ protocol_version: "1.0", results: [{ content, source_url, chunk_id, score }] }`
- 认证：请求头 `Authorization: Bearer <token>` 或 `X-API-Key: <key>`

Rollball 不为各家 RAG 实现 adapter，而是企业侧确保其 RAG 服务兼容此协议。这遵循 PRD "纯对接，不托管" 原则。

**向前兼容设计（Phase 6 演进预留）**：

协议包含 `protocol_version` 字段，为 Phase 6 升级为 MemoryStore 兼容协议预留扩展点。Phase 6 预期演进方向：
- RagClient → RemoteMemoryStore（实现 MemoryStore trait，支持 hybrid_search + graph_expand 完整 API）
- 现有 RAG 服务可降级适配（仅实现 MemoryStore 子集：hybrid_search 返回向量结果，graph_expand 返回空）
- `rag_client: Option<Arc<RagClient>>` → `enterprise_store: Option<Arc<dyn MemoryStore>>` 统一接口
- 此演进不改变 Phase 4 实现，仅约束协议设计：请求/响应 JSON Schema 预留扩展字段、版本字段

**涉及 crate**：`rollball-core`、`rollball-runtime`、`rollball-gateway`

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|---------|
| S4.1 RAG 工具类型定义与配置驱动初始化 | manifest `[[tools]]` 增加 `type = "rag"` 声明；`RagToolConfig` 结构体（endpoint/collection/auth_ref/auth_type/max_results/score_threshold）；`rollball-core/src/manifest.rs` 扩展；Runtime 启动时检测 RAG 配置，条件初始化 `RagClient`，注入到 MemoryManager 和 Tool Dispatcher | 8 | RAG 声明解析、有/无 RAG 配置的 Runtime 初始化分支 |
| S4.2 RAG 标准查询协议与 RagClient | `RagClient` 结构体：标准查询接口（`POST <endpoint>` 请求构造 + JSON 响应解析）；RagClient 是纯 HTTP 客户端，不包含任何 RAG 引擎逻辑；超时 10s，超时/不可达返回空结果；协议含 `protocol_version` 字段（当前 "1.0"），请求/响应 JSON Schema 预留扩展字段（Phase 6 向前兼容） | 10 | 请求构造、响应解析、超时降级、错误处理、协议版本字段存在 |
| S4.3 RAG 认证集成 | 支持 API Key / Bearer Token 两种认证（OAuth 2.0 留 Phase 6）；RAG 认证信息走 Vault 管理（`vault:rag_<name>_key`），不暴露在 manifest 或进程环境；Runtime 启动时通过 Socket 获取 RAG Key（与 LLM API Key 同一握手流程） | 6 | Key 从 Vault 获取、不明文暴露 |
| S4.4 RAG 内置工具（显式触发） | 新增第 16 个内置工具 `rag_query`（仅 manifest 声明 RAG 时注册）：接收 query + 可选 filter/top_k 参数；调用 RagClient 查询；结果标注 source_url / chunk_id；错误时返回友好降级信息 | 8 | 有 RAG 配置时工具可用、无配置时不注册 |
| S4.5 MemoryManager 双通道检索（自动触发） | MemoryManager.retrieve() 增加条件分支：检测 `rag_client: Option<Arc<RagClient>>` → Some 时并行查询 Grafeo + RAG，None 时仅查 Grafeo；RAG 通道用用户消息作 query，top_k=3；结果按来源标注 `[Grafeo]` / `[RAG:<tool_name>]`；RAG 不可达时跳过，不阻塞 | 10 | 有 RAG 配置双通道并行、无 RAG 配置行为不变、RAG 降级 |
| S4.6 RAG 工具权限校验 | manifest 声明 `rag:query` 权限；PermissionChecker 校验 RAG 工具调用；网络权限校验（RAG endpoint URL 必须在 manifest 声明的 `network:<url_pattern>` 白名单内） | 6 | 无 rag 权限拒绝、URL 不在白名单拒绝 |
| S4.7 RAG 集成测试 | Mock RAG Server 端到端：manifest 声明 RAG → 安装 → 自动双通道检索 → 显式 rag_query → RAG 降级；对比测试：无 RAG 声明的 Agent 行为完全不变 | 8 | 全链路通过、无 RAG 零侵入验证 |
| S4.8 RAG 标准查询协议文档 | 编写 RAG 接入指南：标准查询协议（请求/响应 JSON Schema，含 protocol_version 和扩展字段定义）；企业 RAG 自适配示例（Qdrant/Milvus/ES）；认证配置说明；Phase 6 协议演进路线说明 | 2 | 文档完整、示例可运行 |

**里程碑 M18：RAG 工具可用** — 企业 RAG 双通道检索打通，无 RAG 配置的 Agent 零侵入

预期测试合计：58 项

---

### S5：Phase 3 遗留修复与集成验证（2 周，7 项任务）

解决 Phase 3 Code Review 遗留的技术债务，并进行 Phase 4 全系统集成验证。

**涉及 crate**：`rollball-runtime`、`rollball-grafeo`、`rollball-gateway`

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|---------|
| S5.1 Approval Gate CLI 交互式确认 | 使用 `dialoguer` crate 实现真正的 `Confirm::new().interact()` 阻塞确认；`#[cfg(feature = "interactive-cli")]` 条件编译；非交互模式保持 AutoApprove 行为 | 4 | CLI 模式等待用户输入、非交互模式降级 |
| S5.2 WASM ResourceLimiter | 为 Store 添加 `wasmtime::ResourceLimiter` 实现；限制线性内存分配上限（max_memory_mb 配置）；记录 `store.fuel_consumed()` 到审计日志 | 4 | 超限内存分配被拒绝、Fuel 消耗记录 |
| S5.3 权限解析错误改进 | `Permission::parse()` 返回 `Result<Permission, PermissionParseError>` 替代 `Option<Permission>`；错误信息包含非法输入和期望格式 | 4 | 解析失败返回错误详情 |
| S5.4 FsWatcher 异步通道 | `std::sync::mpsc` 替换为 `tokio::sync::mpsc`；watcher 事件在 async Runtime 中非阻塞处理 | 4 | 异步事件接收、不阻塞 Runtime |
| S5.5 三元组去重重命名 | `is_duplicate()` 重命名为 `is_potential_conflict()`；补充注释说明检测逻辑（同一 subject+predicate 的不同 object 值应触发冲突处理） | 2 | 重命名+注释完成 |
| S5.6 HTTP + Permission + Intent + Cron + RAG 集成测试 | 端到端场景：HTTP API 安装 Agent → 启动 → Cron 触发 → Agent 通过 rag_query 检索 → Intent 跨 Agent → Permission 请求 → 流式对话 | 6 | 全链路通过 |
| S5.7 性能基准测试 | HTTP API P99 延迟；Permission IPC 单次请求延迟；RAG 查询延迟；Cron 触发精度 | 4 | 指标可量化、无性能回归 |

**里程碑 M19：Phase 4 交付** — HTTP API + Permission IPC + Cron + RAG 全链路验证通过

预期测试合计：28 项

---

## 总览

| 阶段 | 主题 | 任务数 | 预期测试 | 预计周期 |
|------|------|--------|---------|---------|
| S1 | Gateway HTTP API（Axum） | 9 | 70 | 4 周 |
| S2 | Permission IPC 协议与 Intent 权限校验 | 6 | 38 | 2 周 |
| S3 | Cron 触发器集成 | 5 | 34 | 2 周 |
| S4 | RAG 工具集成（配置驱动 opt-in） | 8 | 58 | 3 周 |
| S5 | Phase 3 遗留修复与集成验证 | 7 | 28 | 2 周 |
| **合计** | | **35** | **228** | **12~14 周** |

---

## 依赖关系

```
S1（HTTP API）──┬──→ S2（Permission IPC）──→ S5.6
               ├──→ S3（Cron）────────────→ S5.6
               └──→ S4（RAG）─────────────→ S5.6
S5.1~S5.5（Phase 3 遗留）───────────────→ S5.6
```

- S1 是 S2/S3 的前置（HTTP API 为 Permission/Cron 提供管理接口）
- S1 是 S4 的前置（RAG 工具的权限管理走 HTTP API）
- S2 依赖 S1 中 HTTP Server 框架就绪后才能添加 Permission HTTP 端点
- S3 的 Cron 管理 API 依赖 S1 的 HTTP Server
- S4 独立于 S2/S3（RAG 工具是独立检索通道），但依赖 S1 的 HTTP 基础设施（Vault HTTP API 用于 RAG Key 管理）
- S5.1~S5.5 是 Phase 3 遗留修复，相互独立，可与 S2~S4 并行
- S5.6 集成测试依赖 S1~S4 全部完成

---

## 关键技术决策点

| 编号 | 决策项 | 决策 | 理由 |
|------|--------|------|------|
| D1 | HTTP API 框架 | **Axum** | Rust 生态最成熟的 HTTP 框架；Gateway 设计文档已确认；与 tokio 运行时无缝集成 |
| D2 | HTTP API 认证策略 | **可选 Auth Token（Phase 4 基础版）** | localhost only 天然限制访问范围；token 机制为 Desktop App 提供基本保护；OAuth 等复杂认证留给 Phase 6 |
| D3 | RAG 与 Grafeo 的架构关系 | **双通道独立，不统一抽象** | PRD ADR-001：Grafeo 是图数据库（关联扩散/遗忘衰减），RAG 是向量检索（批量查询/无状态），两者查询范式和存储模型完全不同 |
| D9 | RAG 触发模型 | **混合双触发（自动 + 显式）** | 自动触发（MemoryManager Retrieve）解决"LLM 不知道该不该查"；显式触发（tool_call）解决"需要更精确查询"；均由 manifest 配置驱动 opt-in（ADR-012） |
| D10 | RAG 协议适配方向 | **Rollball 定义标准协议，企业 RAG 自适配** | 不为各家 RAG 实现 adapter，企业侧确保其服务兼容标准查询接口；遵循 PRD "纯对接，不托管"原则 |
| D4 | Permission Request 阻塞策略 | **独立 task + 超时 60s** | Gateway 主循环不能被权限请求阻塞（其他 Agent 的 IPC 请求需继续处理）；Runtime 侧 60s 超时避免无限等待 |
| D5 | Cron 持久化选型 | **rusqlite** | Cron 条目需要事务性写入和查询；数据量小（每个 Agent 通常 1-5 条）；与 PermissionStore 同技术栈 |
| D6 | RAG 认证信息存储 | **Vault 统一管理** | 与 LLM API Key 同一安全基线：不明文暴露、一次性分发、secrecy::SecretString 存储 |
| D7 | Approval Gate CLI 交互 | **dialoguer + feature gate** | `#[cfg(feature = "interactive-cli")]` 条件编译；CI 环境和 headless 模式用 AutoApprove；用户交互式模式用 dialoguer |
| D8 | HTTP API 端口策略 | **19876 默认 + 冲突递增 + pidfile 记录** | 04-gateway.md §9.2 设计；pidfile 供 Desktop App 发现；递增上限 3 次（19876~19878） |

---

## 与 PRD 需求的映射

### P0/P1 需求覆盖

| PRD 需求 | Phase 4 任务 | 说明 |
|---------|-------------|------|
| COM-03 HTTP API（REST + WebSocket）P1 | S1.1~S1.9 | 完整实现 |
| GTW-08 HTTP API（Axum, 端口 19876）P2 | S1.1, S1.8 | 完整实现 |
| GTW-10 定时触发器（cron 解析）P3 | S3.1~S3.5 | 完整实现 |
| RAG-01 manifest 声明 rag 工具 P2 | S4.1 | 完整实现，配置驱动 opt-in |
| RAG-02 标准查询接口 P2 | S4.2 | 完整实现，Rollball 定义协议，企业 RAG 自适配 |
| RAG-03 企业认证 P2 | S4.3 | 完整实现（API Key / Bearer Token；OAuth 2.0 留 Phase 6） |
| RAG-04 Vault 管理 RAG 认证 P2 | S4.3 | 完整实现 |
| RAG-05 查询结果标注来源 P2 | S4.4, S4.5 | 完整实现（自动 + 显式两种触发均标注） |
| RAG-07 离线降级 P2 | S4.5 | 完整实现，RAG 不可达时跳过，不阻塞 Agent |
| GTW-01 Gateway 纯基础设施 P0 | S1, S2, S3 | HTTP API 是薄封装层，不引入业务逻辑 |
| SEC-04 权限声明 P0 | S2.4 | Intent 权限校验 |

### RAG-06 暂不实现

RAG-06（多租户隔离 namespace/collection/index 约束）为 P3 需求，延后至 Phase 6 云端生态阶段。

---

## 与后续 Phase 的关系

- **Phase 5（Desktop App + 开发框架）**：S1 HTTP API 是 Desktop App 的硬前置；S2 Permission HTTP API 为权限管理 UI 提供接口；S3 Cron HTTP API 为定时任务管理 UI 提供接口
- **Phase 5（Debug Protocol）**：HTTP API 的 WebSocket 机制为 Debug Protocol（JSON-RPC 2.0 over WebSocket）提供参考实现
- **Phase 6（云端与生态）**：RAG 认证的 OAuth 2.0 完整实现；RAG-06 多租户隔离；远程仓库 API
- **Phase 7（跨平台）**：HTTP API 无平台差异（纯 HTTP），跨平台无需适配

---

## 风险评估

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| Axum 与 Gateway 共享状态的死锁 | 中 | S1 阻塞 | 使用 `Arc<RwLock>` 统一锁策略；HTTP handler 只做短时读/写；长操作（Agent 启动）释放锁后异步执行 |
| HTTP WebSocket 与 Socket IPC 的消息一致性 | 中 | S1.6 消息丢失/重复 | 统一消息路由层：HTTP 和 Socket 共享 `SessionManager`；消息 ID 全局唯一 |
| RAG 服务接口不兼容 | 中 | S4 无法对接 | Rollball 定义标准查询协议（请求/响应 JSON Schema），企业 RAG 自行适配此协议；提供接入指南和示例；不实现各家 RAG 的 adapter |
| Permission Request CLI 阻塞用户体验 | 低 | S2 用户等待 | 默认 60s 超时；已授权权限自动批准（缓存命中）；Desktop App 阶段提供 GUI 确认 |
| Cron 触发器与 Agent 生命周期的竞态 | 低 | S3 Agent 未就绪收到 Intent | 触发后等待 Agent IPC 握手完成再推送（复用 Phase 2 S4.1.2a 就绪判断逻辑） |
| RAG 查询延迟影响对话响应 | 中 | S4 用户体验差 | RAG 通道设置独立超时（默认 5s）；超时后降级为纯 Grafeo 通道；并行查询不串行等待 |

---

## 设计决策记录（ADR）

### ADR-012：RAG 触发模型——混合双触发，配置驱动 Opt-In

**状态**：已接受

**上下文**：

RAG 工具需要融入 Runtime 主循环，但存在两个设计问题：
1. 触发方式：LLM 显式调用（tool_call）vs 自动注入（MemoryManager Retrieve），还是两者兼有？
2. 默认行为：RAG 是所有 Agent 的默认能力，还是特定 Agent opt-in 的扩展？

**决策**：

1. **混合双触发**：
   - 自动触发（步骤② MemoryManager Retrieve）：每轮迭代用用户消息作轻量查询（top_k=3），解决"LLM 不知道该不该查企业知识"的问题
   - 显式触发（步骤⑤ Tool Dispatch）：LLM 通过 tool_call 主动调用 RAG 工具，用于针对性深入查询

2. **配置驱动 Opt-In**：
   - RAG 不是默认能力，仅当 manifest 声明 `[[tools]] type = "rag"` 时使能
   - 无 RAG 声明的 Agent，MemoryManager.retrieve() 仅查 Grafeo，Tool Dispatcher 不注册 RAG 工具，行为与 Phase 3 完全一致
   - 有 RAG 声明的 Agent，Runtime 启动时条件初始化 RagClient，注入到 MemoryManager 和 Tool Dispatcher

3. **Rollball 不实现 RAG，只定义标准查询协议**：
   - 请求格式：`POST <endpoint>`，body: `{ query, collection?, top_k, score_threshold?, filters? }`
   - 响应格式：`[{ content, source_url, chunk_id, score }]`
   - 企业 RAG 自行适配此协议，Rollball 不为各家 RAG 实现 adapter

**理由**：

- 双触发：自动触发保证企业知识"天然可用"（LLM 无需主动判断），显式触发保证"需要时深入查询"
- Opt-In：RAG 是企业级 Agent 的特殊需求，不应影响普通 Agent 的行为和性能
- 不实现 adapter：遵循 PRD "纯对接，不托管"原则，避免为每家 RAG 维护适配代码

**后果**：

- 得：企业 Agent 自动获得双通道检索能力；普通 Agent 零侵入；协议简单，企业适配成本低
- 失：MemoryManager 需要条件分支（`rag_client: Option<Arc<RagClient>>`）；自动 RAG 查询增加每轮延迟（超时 5s 上限）；企业需自行适配标准协议

### ADR-009：RAG 与 Grafeo 的架构关系——双通道独立

**状态**：已接受

**上下文**：

Agent 需要同时查询本地 Grafeo（个人记忆）和企业 RAG（集体知识）的检索能力。有两种架构选择：
- A：统一抽象为单一 MemoryStore 接口
- B：双通道独立，结果拼接送入 LLM

**决策**：

采用方案 B——双通道独立，不统一抽象。理由遵循 PRD ADR-001：
1. Grafeo 是图数据库（支持关联扩散、遗忘衰减、图遍历）
2. RAG 是向量检索（批量查询、无状态、无图操作）
3. 两者查询范式和存储模型完全不同
4. 强行统一抽象会引入不必要的复杂度
5. RAG 的多租户隔离、数据写入权限与 Grafeo 的模型不兼容

**后果**：
- 得：架构侵入最小、隐私边界清晰、企业自主可控
- 失：LLM 需要同时处理两种来源的检索结果；需要拼接策略和 token 预算分配

### ADR-010：HTTP API 认证策略——可选 Auth Token

**状态**：已接受

**上下文**：

Gateway HTTP API 监听 localhost only，天然限制访问范围。但 Desktop App 需要确认请求来自合法客户端而非恶意本地程序。

**决策**：

Phase 4 实现基础 Auth Token 机制：
1. Gateway 启动时生成随机 256-bit token
2. Token 写入 `~/.config/agent-gateway/http_token`（文件权限 0600）
3. Desktop App 首次连接时读取该文件，后续请求携带 `Authorization: Bearer <token>`
4. 认证默认关闭（`auth_enabled: false`），可在 config.toml 启用
5. `/health` 端点不需要认证

Phase 6 再考虑 OAuth 2.0 / mTLS 等企业级认证。

**权衡**：
- 得：简单可靠、零外部依赖、Desktop App 快速接入
- 失：token 文件泄露风险（与 localhost 同等风险量级）；不支持多用户场景

### ADR-011：Cron 持久化选型——rusqlite

**状态**：已接受

**上下文**：

Cron 触发器条目需要在 Gateway 重启后恢复。存储选型有三个选择：
- A：纯内存 + JSON 文件
- B：rusqlite
- C：复用 Grafeo

**决策**：

选择方案 B（rusqlite）。理由：
1. Cron 数据需要事务性写入（注册/取消/更新必须原子）
2. 数据量小（每个 Agent 通常 1-5 条），rusqlite 足够
3. 与 PermissionStore 同技术栈，降低维护成本
4. 不选 A：JSON 文件无事务保护，并发写入可能损坏
5. 不选 C：Cron 数据不需要向量检索/图遍历，Grafeo 过重

---

## 附录：S1 HTTP API 路由定义参考

来自 `docs/04-gateway.md` §9.3，Phase 4 完整实现：

```rust
pub fn http_routes() -> Router<GatewayState> {
    Router::new()
        .route("/health", get(health_check))
        // --- Agent 管理 ---
        .route("/api/agents", get(list_agents))
        .route("/api/agents/:id", get(get_agent_detail))
        .route("/api/agents/install", post(install_agent))
        .route("/api/agents/:id", delete(uninstall_agent))
        .route("/api/agents/:id/start", post(start_agent))
        .route("/api/agents/:id/stop", post(stop_agent))
        // --- 对话 ---
        .route("/api/agents/:id/message", post(send_message))
        .route("/api/agents/:id/stream", get(agent_stream_ws))
        // --- Vault ---
        .route("/api/vault/keys", get(list_keys))
        .route("/api/vault/keys", post(add_key))
        .route("/api/vault/keys/:provider", delete(remove_key))
        .route("/api/vault/keys/:provider", put(update_key))
        // --- 配置 ---
        .route("/api/config", get(get_config))
        .route("/api/config", put(update_config))
        // --- 系统信息 ---
        .route("/api/status", get(system_status))
        // --- 权限（S2 新增）---
        .route("/api/agents/:id/permissions", get(list_permissions))
        .route("/api/agents/:id/permissions/:perm/grant", post(grant_permission))
        .route("/api/agents/:id/permissions/:perm", delete(revoke_permission))
        // --- Cron（S3 新增）---
        .route("/api/agents/:id/cron", get(list_crons))
        .route("/api/agents/:id/cron", post(add_cron))
        .route("/api/agents/:id/cron/:cron_id", delete(remove_cron))
}
```

WebSocket 消息格式（对话流式）：

```json
// Client → Server
{ "type": "message", "content": "北京今天天气怎么样" }

// Server → Client
{ "type": "chunk", "delta": "今", "message_id": "msg-001" }
{ "type": "chunk", "delta": "天", "message_id": "msg-001" }
{ "type": "tool_call", "name": "http_request", "params": {...} }
{ "type": "tool_result", "name": "http_request", "result": {...} }
{ "type": "done", "message_id": "msg-001", "usage": {...} }
```
